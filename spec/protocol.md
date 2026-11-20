# The provisioning plugin protocol

How ferrobill and a provisioning plugin talk. A plugin is a small web service, in any language,
that ferrobill calls with signed JSON when a customer's service starts, is suspended or
unsuspended, moves to another package, or is cancelled. The same JSON also works for a
WebAssembly plugin loaded into ferrobill (`plugin.wit`, the `provision` op).

Parts marked **(ferrobill 0.8)** are not sent by ferrobill 0.7 yet. Plugins should work without
them, and the SDK's types already accept both.

## Setting a plugin up in ferrobill

1. Run the plugin where ferrobill can reach it, for example next to it in the same cluster.
   Nothing but ferrobill needs to reach it.
2. Register it: `POST /v1/plugins {"name": "DirectAdmin", "url": "http://plugin:8080"}`. The
   answer holds the shared **secret** once. Give the plugin that secret
   (`FERROBILL_PLUGIN_SECRET`).
3. `POST /v1/plugins/{id}/check`: ferrobill fetches the manifest, which proves both sides have
   the same secret.
4. Point a product at it: `PUT /v1/products/{id}/provisioning {"plugin_id": "...", "config":
   {"package": "basic"}}`. `config` is any JSON object, passed on with every job.

## Signatures

Every call from ferrobill carries

```
ferrobill-signature: t=<unix seconds>,v1=<hex HMAC-SHA256(secret, "<t>.<raw body>")>
```

A GET signs an empty body. Recompute the HMAC over the raw body, compare in constant time, and
refuse a timestamp more than 5 minutes from your clock (answer 401). The SDK does this for you.

## `GET /manifest`

```json
{"name": "directadmin", "version": "0.1.0", "kinds": ["provisioning"],
 "service_fields": [
   {"name": "domain", "label": "Domain", "required": true, "kind": "domain"},
   {"name": "username", "label": "Username", "kind": "username",
    "help": "Leave empty to make one from the domain"}],
 "actions": [{"name": "reset_password", "label": "Reset the control panel password", "client": true}],
 "servers": ["da1", "da2"]}
```

- `name`, `version` and `kinds` are required. `kinds` holds `provisioning`.
- `service_fields` **(ferrobill 0.8)**: what a service of this plugin needs. ferrobill asks for
  them when the service is created (API, admin, client order), checks them (`kind`: `text`,
  `domain`, `username`) and stores them in the service's `metadata`.
- `actions` **(ferrobill 0.8)**: things that can be run on demand (`POST /action`). `client:
  true` offers the action in the client area as well as to admins.
- `servers` **(ferrobill 0.8)**: the servers this plugin is set up for, by name. **Server
  credentials stay with the plugin**, never in ferrobill. ferrobill only knows a server's name,
  group and capacity.

## `POST /provision`

```json
{"job_id": "01a1...", "action": "create",
 "service": {"id": "01a1...", "label": "acme.example", "quantity": 1, "status": "active",
             "metadata": {"domain": "acme.example"}, "provisioning": {}},
 "customer": {"id": "01a1...", "name": "Acme Ltd", "email": "ap@acme.example"},
 "product": {"id": "01a1...", "name": "Web hosting"},
 "config": {"package": "basic"},
 "server": {"name": "da1"}}
```

| Service change | `action` |
|---|---|
| created active, or activated | `create` |
| suspended (by hand or for an unpaid invoice) | `suspend` |
| unsuspended | `unsuspend` |
| moved to another product on the same plugin **(ferrobill 0.8)** | `change_package` (`config` is the new product's, `previous_config` the old one's) |
| cancelled | `terminate` |

- `server` **(ferrobill 0.8)**: for `create`, the server ferrobill picked from the product's
  server group (least full first). For later jobs, the server the service is on. Until then,
  plugins fall back to what their own `create` answered (`service.provisioning.server`).
- `service.provisioning` holds what earlier jobs answered, merged, so `suspend` knows the account
  `create` made.

Answers:

- **2xx**: done. Optionally `{"data": {"username": "acme1", "server": "da1"}}`. That object is
  merged into the service's provisioning data. Never put passwords in it.
- **422** `{"detail": "..."}`: refused. The job fails at once and the admin sees `detail`; they
  can retry it by hand after fixing the cause.
- Anything else, or no answer within 30 seconds: retried (1 min ... 12 h, eight tries).

**The same job may arrive more than once** (ferrobill timed out after the work was done), and a
service's jobs arrive in order, each after the one before it is done. Make every action
idempotent: a `create` for an account that already exists for this service is done, a `suspend`
of a suspended account is done, a `terminate` of an account already gone is done.

## `POST /action` **(ferrobill 0.8)**

```json
{"action": "reset_password", "by": "client",
 "service": {...}, "customer": {...}, "product": {...}, "config": {...},
 "server": {"name": "da1"}}
```

Answer 2xx with any of:

```json
{"redirect_url": "https://da1.example.com:2222/...",
 "message": "New password for acme1.",
 "secret": "a-new-password",
 "data": {"...": "..."}}
```

ferrobill sends the person to `redirect_url` (single sign-on), shows `message`, shows `secret`
once and never stores it, and merges `data` into the provisioning data. 422 refuses, with
`detail` shown to the person.

## `GET /healthz`

Not signed. `200 ok` for liveness and readiness probes.
