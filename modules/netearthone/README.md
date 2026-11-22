# NetEarthOne registrar plugin (`ferrobill-netearthone`)

For domains in a NetEarthOne reseller account: ferrobill lists the account's domains (to import
them), reads each domain's expiry date, and renews the ones that are due (their renewal invoice
paid, or renewed by hand in ferrobill). ferrobill never renews a domain by itself, and not at all
while its `domain_dry_run` setting is on.

NetEarthOne runs on LogicBoxes, so this uses the LogicBoxes (ResellerClub) HTTP API at
`https://httpapi.com/api` (test: `https://test.httpapi.com/api`).

| ferrobill operation | NetEarthOne API |
|---|---|
| `domains` | `GET domains/search.json`, 500 orders a page (the cursor is the next page number); deleted and archived orders left out |
| `info` | `GET domains/orderid.json` (the order of the domain name), then `GET domains/details.json` with `options=OrderDetails` (`endtime`) |
| `renew` | the same two reads, then `POST domains/renew.json` with `exp-date` set to the current expiry time, then `details.json` again for the new date |

Expiry times are unix seconds; they are given to ferrobill as the UTC date.

A renewal is **safe to repeat**: the expiry date is read first. If it is already later than the
date ferrobill asked about, the domain was renewed (a retry after a lost answer, or renewed
elsewhere) and that date is the answer; if it is earlier, the renewal is refused. If it is the
same day, the renewal is sent with `exp-date` = the order's exact current expiry time, which the
API checks: once a renewal has moved the date, another from the old date is refused.

The renewal is sent with `invoice-option=NoInvoice`: no invoice is raised for the customer and
the order is renewed, paid from the reseller account's balance (ferrobill does the invoicing).
`auto-renew` (required by the API) is sent as the order already has it, so it is not changed. If
the expiry date has not moved a few seconds after the renewal was accepted, the plugin does not
guess the new date: it answers refused with the action's status and ID, so that ferrobill doesn't
send it again by itself and someone checks the order.

Errors: the API answers its own errors (an unknown domain, a wrong `exp-date`, a wrong key) as
`{"status": "ERROR", ...}`, often with HTTP 500; those are refused with the API's message.
Network errors and other 5xx answers are retried; other 4xx answers are refused. The API key
travels as a request parameter (in the query string on reads), so error messages never contain a
URL, and the key is blanked out of anything the API answers.

Tested against a fake of the API (listing with pages, info, renewals and repeats, refusals,
error mapping, dates), and read-only against the live API (the ignored `live_read_only` test:
search, orderid and details only). A renewal has not been run against the live API.

## Setting it up

1. In the NetEarthOne control panel: Settings, API: note your reseller ID (`auth-userid`), get
   the API key, and add the public IP address the plugin's traffic leaves from to the allowed
   addresses: the API accepts calls only from listed addresses. Keep funds in the reseller
   account: renewals are paid from it.
2. Run the image with:
   - `FERROBILL_PLUGIN_SECRET`: the secret ferrobill gave when the plugin was registered
   - `NETEARTHONE_AUTH_USERID`, `NETEARTHONE_API_KEY`
   - `NETEARTHONE_URL` (optional): `live` (default), `test`, or a base URL ending `/api`
   - `LISTEN` (default `0.0.0.0:8080`)
3. In ferrobill: register it (`POST /v1/plugins`), check it, then set it as the registrar plugin
   of your NetEarthOne domains, or import them all from it (`POST /v1/plugins/{id}/import-domains`).

The image has a CA bundle for the API's TLS certificate and nothing else (`scratch`).
