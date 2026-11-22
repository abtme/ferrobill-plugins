# Cloudflare Registrar plugin (`ferrobill-cloudflare`)

For domains registered with Cloudflare Registrar: ferrobill imports the account's domains and
keeps each one's expiry date current. **Listing and expiry dates only**: Cloudflare renews its
domains itself, charging the account's payment method up to 30 days before expiry when the
domain's auto-renew is on, and its API has no call to renew a domain on demand. So the plugin
doesn't offer `renew`; if ferrobill sends one anyway it is refused (422) with that reason, and
the new date shows at the next `info` after Cloudflare has renewed.

| ferrobill operation | Cloudflare API |
|---|---|
| `domains` | `GET /accounts/{account_id}/registrar/registrations?per_page=50`, paged by Cloudflare's `cursor` ([List Registrations](https://developers.cloudflare.com/api/resources/registrar/subresources/registrations/methods/list/)) |
| `info` | `GET /accounts/{account_id}/registrar/registrations/{domain_name}`: `expires_at` as `YYYY-MM-DD` ([Get Registration](https://developers.cloudflare.com/api/resources/registrar/subresources/registrations/methods/get/)) |
| `renew` | not offered (refused) |

These are Cloudflare's Registrations endpoints. The older Registrar Domains endpoints
(`/registrar/domains`) are deprecated, end of life 27 September 2026.

**Auto-renew**: each registration has an `auto_renew` flag. The plugin protocol has no field for
it, so ferrobill doesn't see it: make sure auto-renew is on in the Cloudflare dashboard (Domain
Registration > Manage Domains) for every domain you bill for, or Cloudflare lets it expire.
(`PATCH .../registrations/{domain_name}` can only set `auto_renew`; the plugin doesn't call it.)

A registration or transfer that is still pending has no expiry date yet: `info` answers "retry"
until it has one, and `domains` lists it without a date.

Errors: no answer, HTTP 429 or 5xx are retried; any other 4xx, or `"success": false`, is refused
with Cloudflare's messages (for example `Domain not found (10000)`, or `Authentication error
(10000)` for a token without Registrar read).

Tested against a fake Cloudflare API (paging, info, not found, pending, error mapping). Not yet
run against a real account's domains: the read-only `live` test (`cargo test -p
ferrobill-cloudflare live -- --ignored`, GETs only) needs a token with Registrar read.

## Setting it up

1. In the Cloudflare dashboard, make an API token (My Profile > API Tokens) for the account with
   read-only access to its Registrar domains (an Account permission for Registrar, Read; nothing
   else is needed), and note the account ID.
2. Run the image with:
   - `FERROBILL_PLUGIN_SECRET`: the secret ferrobill gave when the plugin was registered
   - `CLOUDFLARE_API_TOKEN`, `CLOUDFLARE_ACCOUNT_ID`
   - `CLOUDFLARE_URL` (optional, default `https://api.cloudflare.com/client/v4`), `LISTEN`
     (default `0.0.0.0:8080`)
3. In ferrobill: register it (`POST /v1/plugins`), check it, then import the domains from it
   (`POST /v1/plugins/{id}/import-domains`) or set it as the registrar plugin of your Cloudflare
   domains. Don't let ferrobill renew them: Cloudflare does.

The image has a CA bundle for Cloudflare's TLS certificate and nothing else (`scratch`).
