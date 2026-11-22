# eNom registrar plugin (`ferrobill-enom`)

For domains in an eNom reseller account: ferrobill lists them (to import), reads each domain's
expiry date from eNom and renews the ones that are due (their renewal invoice paid, or renewed by
hand in ferrobill). ferrobill never renews a domain by itself, and not at all while its
`domain_dry_run` setting is on.

| ferrobill operation | eNom reseller API (`interface.asp`) |
|---|---|
| `domains` | `GetDomains` (`Tab=IOwn`, 100 a page; the cursor is eNom's `Start`) |
| `info` | `GetDomainInfo`: `status/expiration` and `status/registrar` |
| `renew` | `GetDomainInfo`, then `Extend` (`NumYears`) only from that exact expiry date, then `GetDomainInfo` again |

A renewal is **safe to repeat**: the expiry date is read first. If it is already later than the
date ferrobill asked about, the domain was renewed (a retry after a lost answer, or renewed
elsewhere) and that date is the answer; if it is earlier, the renewal is refused. eNom itself
also refuses a second renewal of the same domain within 24 hours (the plugin never sends
`OverrideOrder`). If eNom doesn't show the new date straight after `Extend` (and its answer has no
`RegistryExpDate`), the plugin answers "retry": asked again, the date will have moved on.

Errors: eNom answers them inside the XML (`ErrCount`, `<errors><Err1>`, `ResponseNumber`). Bad
logins, an address not allowed, a domain not in the account and invalid values are refused;
eNom's system errors (`ResponseNumber` 6xxxxx, "System error"), anything saying to try again,
HTTP 5xx and network failures are retried.

Tested against a fake eNom server, and **read-only against the live API** (`GetDomains` and
`GetDomainInfo`: the ignored `live_read_only` test). `Extend` has not been run against eNom: it
costs money on the live account. To try it, use eNom's test environment (`ENOM_URL=test`, its
own account and login).

## Setting it up

1. In eNom's reseller control panel: make an API token and add the public IP address the
   plugin's traffic leaves from (eNom answers `713254` "Policy error; unauthorized" from any
   other); the live and test environments are separate.
2. Run the image with:
   - `FERROBILL_PLUGIN_SECRET`: the secret ferrobill gave when the plugin was registered
   - `ENOM_UID`: the account's login ID
   - `ENOM_PW`: its API token (or password)
   - `ENOM_URL` (optional): `live` (the default, `https://reseller.enom.com/interface.asp`),
     `test` (`https://resellertest.enom.com/interface.asp`) or another `interface.asp` URL
   - `LISTEN` (default `0.0.0.0:8080`)
3. In ferrobill: register it (`POST /v1/plugins`), check it, then set it as the registrar plugin
   of your eNom domains, or import them all from it (`POST /v1/plugins/{id}/import-domains`).

The image has a CA bundle for eNom's TLS certificate and nothing else (`scratch`).

## eNom's API documentation

- [GetDomains](https://api.enom.com/docs/get-domains)
- [GetDomainInfo](https://api.enom.com/docs/getdomaininfo)
- [Extend](https://api.enom.com/docs/extend)
- [Error responses](https://support.enom.com/support/solutions/articles/201000065294)
