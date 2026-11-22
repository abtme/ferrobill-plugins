# Gandi registrar plugin (`ferrobill-gandi`)

For domains at Gandi: ferrobill imports the organization's domains, reads each domain's expiry
date from Gandi and renews the ones that are due (their renewal invoice paid, or renewed by hand
in ferrobill). ferrobill never renews a domain by itself, and not at all while its
`domain_dry_run` setting is on. Renewals are paid from the Gandi organization's prepaid account.

Everything is Gandi's v5 REST API ([reference](https://api.gandi.net/docs/reference/),
[domains](https://api.gandi.net/docs/domains/)):

| ferrobill operation | Gandi |
|---|---|
| `domains` | `GET /v5/domain/domains?page=N&per_page=100` (one page per call; the cursor is the next page number, `Total-Count` / `Filtered-Count` tell when it's the last); each item's `fqdn` and `dates.registry_ends_at` |
| `info` | `GET /v5/domain/domains/{fqdn}`: `dates.registry_ends_at` |
| `renew` | `GET /v5/domain/domains/{fqdn}`, then `POST /v5/domain/domains/{fqdn}/renew` with `{"duration": years}` (1 to 9), then the date read again |

A renewal is **safe to repeat**: the expiry date is read first. If it is already later than the
date ferrobill asked about, the domain was renewed (a retry after a lost answer, or renewed
elsewhere) and that date is the answer; if it is earlier, the renewal is refused. Only when it is
the same is the renewal sent.

Gandi accepts a renewal (`202 Accepted`) before it is done, so the plugin reads the date again
(up to 5 times, 2 seconds apart). If it still hasn't moved, the answer is "retry" and ferrobill
asks again later, when the date has moved and the first check answers it. While such a renewal is
pending (up to 24 hours) the plugin doesn't send another for that domain and date. That memory is
in the process: if the plugin restarts while Gandi is still doing a renewal, a repeat in that
window could renew twice, so don't restart it straight after a renewal that answered "retry".

Errors: no answer, 429 (Gandi allows 1000 requests a minute per IP) and 5xx are "retry"; any
other 4xx (a wrong token, an unknown domain, not enough prepaid credit) is refused with Gandi's
message.

Tested against a fake Gandi API (paged listing, info, synchronous and asynchronous renewals,
repeats, refusals, error mapping) and **read-only against the live API** (listing and info: the
ignored `live_read_only` test, which only sends GETs). A renewal has not been run against the
live API or the sandbox.

## Setting it up

1. In the Gandi admin, Organizations, your organization, Sharing: create a personal access
   token allowed to see and renew domains. Gandi's older API keys still
   work but are deprecated: give one as `GANDI_TOKEN="Apikey <key>"` (a value with a space is
   sent as the whole `Authorization` header; without one it is sent as `Bearer <token>`).
2. Run the image with:
   - `FERROBILL_PLUGIN_SECRET`: the secret ferrobill gave when the plugin was registered
   - `GANDI_TOKEN`
   - `GANDI_URL` (optional): `live` (default, `https://api.gandi.net/v5`), `sandbox`
     (`https://api.sandbox.gandi.net/v5`) or an API base URL
   - `GANDI_SHARING_ID` (optional): the organization id that pays for renewals and whose domains
     are listed (`sharing_id`); without it, Gandi uses the token's
   - `LISTEN` (default `0.0.0.0:8080`)
3. In ferrobill: register it (`POST /v1/plugins`), check it, then set it as the registrar plugin
   of your Gandi domains, or import them all from it (`POST /v1/plugins/{id}/import-domains`).

Read-only check against your account (GETs only, nothing renewed):
`GANDI_TOKEN=... cargo test -p ferrobill-gandi live_read_only -- --ignored --nocapture`.
