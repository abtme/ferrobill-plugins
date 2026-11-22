// SPDX-License-Identifier: Apache-2.0
//! Just enough EPP (RFC 5730-5734) for Nominet's .uk registry: login, domain:info,
//! domain:renew, logout, over one TLS connection with RFC 5734 framing (a 4-byte big-endian
//! length that counts itself, then the XML).
//!
//! Learned against Nominet's testbed (the owner's nominet-epp connector): the login must list
//! every object schema the session uses, contact included; declaring an extension the server
//! doesn't offer gets the whole login refused.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const NS_EPP: &str = "urn:ietf:params:xml:ns:epp-1.0";
const NS_DOMAIN: &str = "urn:ietf:params:xml:ns:domain-1.0";
const NS_HOST: &str = "urn:ietf:params:xml:ns:host-1.0";
const NS_CONTACT: &str = "urn:ietf:params:xml:ns:contact-1.0";
/// Frames larger than this are refused (a domain:info answer is a few kB).
const MAX_FRAME: u32 = 1 << 20;

/// Why an EPP exchange failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EppError {
    /// Talking to the server failed (connect, TLS, a broken frame): worth trying again.
    Io(String),
    /// The server answered with an error result: its code (2xxx) and message.
    Result { code: u32, message: String },
}

impl std::fmt::Display for EppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EppError::Io(e) => write!(f, "EPP: {e}"),
            EppError::Result { code, message } => write!(f, "EPP {code}: {message}"),
        }
    }
}

impl EppError {
    /// Whether trying again later could help: connection trouble and the server's own failures
    /// (24xx, 25xx), not refusals of this request (object missing, not ours, wrong date...).
    pub fn retryable(&self) -> bool {
        match self {
            EppError::Io(_) => true,
            EppError::Result { code, .. } => *code >= 2400,
        }
    }
}

fn io(e: impl std::fmt::Display) -> EppError {
    EppError::Io(e.to_string())
}

/// XML text with the five special characters escaped.
pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The text of the first element called `local` (any namespace prefix), unescaped.
pub fn text_of(xml: &str, local: &str) -> Option<String> {
    let mut rest = xml;
    while let Some(i) = rest.find('<') {
        rest = &rest[i + 1..];
        let end = rest.find(|c: char| c == '>' || c.is_whitespace())?;
        let tag = &rest[..end];
        let name = tag.rsplit(':').next().unwrap_or(tag);
        if name == local && !tag.starts_with('/') {
            let open_end = rest.find('>')?;
            if rest[..open_end].ends_with('/') {
                return Some(String::new());
            }
            let body = &rest[open_end + 1..];
            let close = body.find("</")?;
            return Some(unescape(body[..close].trim()));
        }
    }
    None
}

/// The first `<result code="...">` and its `<msg>`.
pub fn result(xml: &str) -> Option<(u32, String)> {
    let at = xml.find("<result")?;
    let tail = &xml[at..];
    let code_at = tail.find("code=\"")? + 6;
    let code: u32 = tail[code_at..].split('"').next()?.parse().ok()?;
    // The message, plus the offending value if the server echoes one (its <msg> alone is
    // sometimes too general to act on).
    let mut message = text_of(tail, "msg").unwrap_or_default();
    if let Some(reason) = text_of(tail, "reason") {
        message = format!("{message} ({reason})");
    }
    Some((code, message))
}

/// A transaction id for `clTRID`.
fn trid() -> String {
    let mut b = [0u8; 8];
    let _ = getrandom::getrandom(&mut b);
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("ferrobill-{hex}")
}

fn command(body: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"no\"?>\
         <epp xmlns=\"{NS_EPP}\"><command>{body}<clTRID>{}</clTRID></command></epp>",
        trid()
    )
}

/// The login command for registrar tag `tag`.
pub fn login_xml(tag: &str, password: &str) -> String {
    command(&format!(
        "<login><clID>{}</clID><pw>{}</pw><options><version>1.0</version><lang>en</lang>\
         </options><svcs><objURI>{NS_DOMAIN}</objURI><objURI>{NS_HOST}</objURI>\
         <objURI>{NS_CONTACT}</objURI></svcs></login>",
        escape(tag),
        escape(password)
    ))
}

pub fn info_xml(name: &str) -> String {
    command(&format!(
        "<info><domain:info xmlns:domain=\"{NS_DOMAIN}\"><domain:name>{}</domain:name>\
         </domain:info></info>",
        escape(name)
    ))
}

/// Renew `name` for `years`, from its current expiry date `from` (`YYYY-MM-DD`: the registry
/// refuses a renewal from any other date, so one can't happen twice by mistake).
pub fn renew_xml(name: &str, from: &str, years: u32) -> String {
    command(&format!(
        "<renew><domain:renew xmlns:domain=\"{NS_DOMAIN}\"><domain:name>{}</domain:name>\
         <domain:curExpDate>{}</domain:curExpDate><domain:period unit=\"y\">{years}\
         </domain:period></domain:renew></renew>",
        escape(name),
        escape(from)
    ))
}

/// Any command body, for the testbed test (domain and contact creation).
#[cfg(test)]
pub(crate) fn test_command(body: &str) -> String {
    command(body)
}

pub fn logout_xml() -> String {
    command("<logout/>")
}

/// One EPP session over any stream (TLS in use, plain TCP in the tests).
pub struct Session<S> {
    stream: S,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Session<S> {
    /// Read the greeting the server sends first.
    pub async fn start(stream: S) -> Result<(Self, String), EppError> {
        let mut s = Session { stream };
        let greeting = s.read().await?;
        Ok((s, greeting))
    }

    async fn read(&mut self) -> Result<String, EppError> {
        let total = self.stream.read_u32().await.map_err(io)?;
        if !(4..=MAX_FRAME).contains(&total) {
            return Err(EppError::Io(format!("bad frame length {total}")));
        }
        let mut buf = vec![0u8; (total - 4) as usize];
        self.stream.read_exact(&mut buf).await.map_err(io)?;
        String::from_utf8(buf).map_err(io)
    }

    async fn write(&mut self, xml: &str) -> Result<(), EppError> {
        let len = u32::try_from(xml.len() + 4).map_err(io)?;
        self.stream.write_u32(len).await.map_err(io)?;
        self.stream.write_all(xml.as_bytes()).await.map_err(io)?;
        self.stream.flush().await.map_err(io)
    }

    /// Send a command; its answer if the result is a success (1xxx).
    pub async fn call(&mut self, xml: &str) -> Result<String, EppError> {
        self.write(xml).await?;
        let answer = self.read().await?;
        match result(&answer) {
            Some((code, _)) if code < 2000 => Ok(answer),
            Some((code, message)) => Err(EppError::Result { code, message }),
            None => Err(EppError::Io("an answer with no <result>".into())),
        }
    }

    pub async fn login(&mut self, tag: &str, password: &str) -> Result<(), EppError> {
        self.call(&login_xml(tag, password)).await.map(|_| ())
    }

    /// A domain's expiry date (`YYYY-MM-DD`) and its registrar (`clID`).
    pub async fn expiry(&mut self, name: &str) -> Result<(String, Option<String>), EppError> {
        let answer = self.call(&info_xml(name)).await?;
        let ex = text_of(&answer, "exDate")
            .ok_or_else(|| EppError::Io("domain:info had no exDate".into()))?;
        Ok((date(&ex)?, text_of(&answer, "clID")))
    }

    /// Renew; the new expiry date (`YYYY-MM-DD`).
    pub async fn renew(&mut self, name: &str, from: &str, years: u32) -> Result<String, EppError> {
        let answer = self.call(&renew_xml(name, from, years)).await?;
        let ex = text_of(&answer, "exDate")
            .ok_or_else(|| EppError::Io("domain:renew had no exDate".into()))?;
        date(&ex)
    }

    /// End the session (Nominet limits sessions per tag); errors are ignored.
    pub async fn logout(mut self) {
        let _ = self.call(&logout_xml()).await;
        let _ = self.stream.shutdown().await;
    }
}

/// `YYYY-MM-DD` from an EPP dateTime (`2027-02-02T12:00:00.0Z`).
fn date(s: &str) -> Result<String, EppError> {
    let d = s.get(..10).unwrap_or("");
    let ok = d.len() == 10
        && d.as_bytes()[4] == b'-'
        && d.as_bytes()[7] == b'-'
        && d.bytes()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit());
    if ok {
        Ok(d.to_owned())
    } else {
        Err(EppError::Io(format!("not a date: {s:?}")))
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! A fake EPP server over plain TCP: greets, then answers each command from `answer`.
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    pub type Seen = Arc<Mutex<Vec<String>>>;

    pub async fn frame(s: &mut (impl AsyncWrite + Unpin), xml: &str) {
        s.write_u32(u32::try_from(xml.len() + 4).unwrap())
            .await
            .unwrap();
        s.write_all(xml.as_bytes()).await.unwrap();
    }

    pub fn ok(inner: &str) -> String {
        format!(
            "<?xml version=\"1.0\"?><epp xmlns=\"{NS_EPP}\"><response><result code=\"1000\">\
             <msg>Command completed successfully</msg></result>{inner}</response></epp>"
        )
    }

    pub fn fail(code: u32, msg: &str) -> String {
        format!(
            "<epp xmlns=\"{NS_EPP}\"><response><result code=\"{code}\"><msg>{msg}</msg></result>\
             </response></epp>"
        )
    }

    /// Serve sessions until the test ends; returns the address and every command received.
    pub async fn serve(answer: impl Fn(&str) -> String + Send + Sync + 'static) -> (String, Seen) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let seen: Seen = Arc::default();
        let answer = Arc::new(answer);
        let s2 = seen.clone();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = l.accept().await.unwrap();
                let (seen, answer) = (s2.clone(), answer.clone());
                tokio::spawn(async move {
                    frame(
                        &mut sock,
                        "<epp><greeting><svID>fake</svID></greeting></epp>",
                    )
                    .await;
                    while let Ok(n) = sock.read_u32().await {
                        let mut b = vec![0u8; (n - 4) as usize];
                        sock.read_exact(&mut b).await.unwrap();
                        let cmd = String::from_utf8(b).unwrap();
                        seen.lock().unwrap().push(cmd.clone());
                        let a = answer(&cmd);
                        frame(&mut sock, &a).await;
                    }
                });
            }
        });
        (addr, seen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_is_built_escaped_and_read_by_local_name() {
        let x = login_xml("ABTME", "p<&>\"'");
        assert!(x.contains("<pw>p&lt;&amp;&gt;&quot;&apos;</pw>"), "{x}");
        assert!(
            x.contains(NS_CONTACT),
            "the contact schema is declared at login"
        );
        let r = renew_xml("a.uk", "2027-02-02", 2);
        assert!(r.contains("<domain:curExpDate>2027-02-02</domain:curExpDate>"));
        assert!(r.contains("<domain:period unit=\"y\">2</domain:period>"));

        let answer = "<epp><response><result code=\"2306\"><msg>Parameter value policy error</msg>\
                      <extValue><value/><reason>curExpDate mismatch</reason></extValue></result>\
                      </response></epp>";
        assert_eq!(
            result(answer),
            Some((
                2306,
                "Parameter value policy error (curExpDate mismatch)".into()
            ))
        );
        let info = "<x:infData><x:name>a.uk</x:name><x:clID>ABTME</x:clID>\
                    <x:exDate>2027-12-18T12:00:00.0Z</x:exDate></x:infData>";
        assert_eq!(
            text_of(info, "exDate").as_deref(),
            Some("2027-12-18T12:00:00.0Z")
        );
        assert_eq!(text_of(info, "clID").as_deref(), Some("ABTME"));
        assert_eq!(text_of(info, "nothing"), None);
        assert_eq!(date("2027-12-18T12:00:00.0Z"), Ok("2027-12-18".into()));
        assert!(date("18 Dec 2027").is_err());
    }

    #[tokio::test]
    async fn a_session_logs_in_reads_renews_and_logs_out() {
        let (addr, seen) = fake::serve(|cmd| {
            if cmd.contains("<login>") {
                fake::ok("")
            } else if cmd.contains("<domain:info") {
                fake::ok("<resData><domain:infData xmlns:domain=\"d\"><domain:name>a.uk</domain:name>\
                          <domain:clID>ABTME</domain:clID><domain:exDate>2027-02-02T00:00:00.0Z\
                          </domain:exDate></domain:infData></resData>")
            } else if cmd.contains("<domain:renew") {
                fake::ok("<resData><domain:renData xmlns:domain=\"d\"><domain:name>a.uk</domain:name>\
                          <domain:exDate>2029-02-02T00:00:00.0Z</domain:exDate></domain:renData></resData>")
            } else {
                fake::ok("")
            }
        })
        .await;
        let sock = tokio::net::TcpStream::connect(&addr).await.unwrap();
        let (mut s, greeting) = Session::start(sock).await.unwrap();
        assert!(greeting.contains("greeting"));
        s.login("ABTME", "pw").await.unwrap();
        assert_eq!(
            s.expiry("a.uk").await.unwrap(),
            ("2027-02-02".into(), Some("ABTME".into()))
        );
        assert_eq!(
            s.renew("a.uk", "2027-02-02", 2).await.unwrap(),
            "2029-02-02"
        );
        s.logout().await;
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 4);
        assert!(seen[3].contains("<logout/>"));
    }

    #[tokio::test]
    async fn errors_say_whether_to_retry() {
        let (addr, _) = fake::serve(|cmd| {
            if cmd.contains("<login>") {
                fake::ok("")
            } else {
                fake::fail(2303, "Object does not exist")
            }
        })
        .await;
        let sock = tokio::net::TcpStream::connect(&addr).await.unwrap();
        let (mut s, _) = Session::start(sock).await.unwrap();
        s.login("ABTME", "pw").await.unwrap();
        let e = s.expiry("nope.uk").await.unwrap_err();
        assert_eq!(
            e,
            EppError::Result {
                code: 2303,
                message: "Object does not exist".into()
            }
        );
        assert!(!e.retryable());
        assert!(
            EppError::Result {
                code: 2502,
                message: "Session limit exceeded".into()
            }
            .retryable()
        );
        assert!(EppError::Io("reset".into()).retryable());
    }
}
