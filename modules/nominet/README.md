# Nominet registrar plugin (`ferrobill-nominet`)

For .uk domains on your own Nominet registrar tag: ferrobill reads each domain's expiry date
from Nominet and renews the ones that are due (their renewal invoice paid, or renewed by hand
in ferrobill). ferrobill never renews a domain by itself, and not at all while its
`domain_dry_run` setting is on.

| ferrobill operation | Nominet |
|---|---|
| `info` | EPP `domain:info`: the expiry date and the registrar tag |
| `renew` | EPP `domain:info`, then `domain:renew` from that exact expiry date |
| `domains` | the List API (`/rest/domains/all`), only if `NOMINET_LIST_API_KEY` is set |

A renewal is **safe to repeat**: the expiry date is read first. If it is already later than the
date ferrobill asked about, the domain was renewed (a retry after a lost answer, or renewed
elsewhere) and that date is the answer; if it is earlier, the renewal is refused; and the
registry itself refuses a renewal from any other date than the current one. Each call is one EPP
session (login, command, logout).

Tested against a fake EPP server and **live against Nominet's testbed** (a throwaway domain
registered there, read, renewed, and renewed again without renewing twice: the ignored
`testbed` test). Not yet run against the live registry.

## Setting it up

1. In Nominet Online Services, for your tag: set an EPP password (testbed and live are
   separate) and add the public IP address the plugin's traffic leaves from: Nominet accepts EPP
   only from listed addresses. For `domains`, make a List API key.
2. Run the image with:
   - `FERROBILL_PLUGIN_SECRET`: the secret ferrobill gave when the plugin was registered
   - `NOMINET_EPP`: `testbed`, `live`, or a `host:port`
   - `NOMINET_TAG`, `NOMINET_EPP_PASSWORD`
   - `NOMINET_LIST_API_KEY` (optional), `LISTEN` (default `0.0.0.0:8080`)
3. In ferrobill: register it (`POST /v1/plugins`), check it, then set it as the registrar plugin
   of your .uk domains, or import them all from it (`POST /v1/plugins/{id}/import-domains`).

The image has a CA bundle for Nominet's TLS certificate and nothing else (`scratch`).
