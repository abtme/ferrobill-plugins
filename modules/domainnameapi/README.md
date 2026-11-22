# Domain Name API registrar plugin (`ferrobill-domainnameapi`)

For domains at [Domain Name API](https://www.domainnameapi.com/) on your reseller account:
ferrobill imports the account's domains, reads each one's expiry date, and renews the ones that
are due (their renewal invoice paid, or renewed by hand in ferrobill). ferrobill never renews a
domain by itself, and not at all while its `domain_dry_run` setting is on.

| ferrobill operation | Domain Name API (REST, `https://api.domainresellerapi.com/api/v1`) |
|---|---|
| `domains` | `GET /domains?MaxResultCount=200&SkipCount=N`, 200 a page |
| `info` | `GET /domains/info?DomainName=...`: only the expiry date is read |
| `renew` | `GET /domains/info`, then `POST /domains/renew` `{"domainName", "period"}`, then `GET /domains/info` again |

Every call carries the `X-API-KEY` and `__reseller` headers. The endpoints are those of the API's
own description (`https://api.domainresellerapi.com/swagger/v1/swagger.json`) and of its official
PHP client ([domainreseller/php-dna](https://github.com/domainreseller/php-dna),
`DomainNameApi/DNARest.php` and its `conformance/fixtures/renew.*.json`).

A renewal is **safe to repeat**: the expiry date is read first. If it is already later than the
date ferrobill asked about, the domain was renewed (a retry after a lost answer, or renewed
elsewhere) and that date is the answer, without renewing again; if it is earlier, the renewal is
refused (check the domain). After renewing, the date is read again (three times, a few seconds
apart) until it has moved; if it hasn't, the renewal's own answer gives the new date, and with
neither the call is answered "retry later".

Errors: no answer, HTTP 5xx, 408 and 429 are retried; any other failure (unknown domain, wrong
key, insufficient balance, a renewal answered `"success": false`) is refused with the API's
message.

The API has no call to set a domain's renewal mode (auto-renew): keep the domains on
`AutoExpire` at Domain Name API and let ferrobill decide when to renew.

`GET /domains/info` also answers the domain's contacts: the plugin reads only the expiry date
and never logs or returns anything else from it.

Tested against a fake of the API, and **read-only against the live API** (the ignored
`live_read_only` test: lists the domains and checks `info` agrees; it never renews). Renewal has
not been run against the live API, since it costs money.

## Setting it up

1. In the Domain Name API reseller panel, make an API key and note your reseller id (a UUID).
2. Run the image with:
   - `FERROBILL_PLUGIN_SECRET`: the secret ferrobill gave when the plugin was registered
   - `DNA_API_KEY`, `DNA_RESELLER_ID`
   - `DNA_URL` (optional): `live` (the default), `ote` (the test system,
     `https://ote.domainresellerapi.com/api/v1`) or a base URL
   - `LISTEN` (default `0.0.0.0:8080`)
3. In ferrobill: register it (`POST /v1/plugins`), check it, then set it as the registrar plugin
   of those domains, or import them all from it (`POST /v1/plugins/{id}/import-domains`).
