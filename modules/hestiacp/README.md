# HestiaCP plugin (`ferrobill-hestiacp`)

One HestiaCP user per ferrobill service, with the service's domain as its first web domain.

| ferrobill job | HestiaCP |
|---|---|
| `create` | `v-add-user` with the product's `package`, then `v-add-domain` (web, DNS and mail) |
| `suspend` / `unsuspend` | `v-suspend-user` / `v-unsuspend-user`, only if not already so |
| `change_package` | `v-change-user-package` |
| `terminate` | `v-delete-user` (its files, mail and databases go too) |

Actions: `reset_password` (a new password, shown once, never stored by ferrobill) and `panel`
(opens the server's control panel). Single sign-on is planned.

Every job first lists the users (`v-list-users json`), so a job sent twice does nothing the
second time; a `create` that stopped after making the user is finished by its retry (only when
that user has no domain and the same contact email and name the plugin gives it, so an account
someone else made under the chosen name is refused, never taken over). The
username is the service's `username` field, or is made from the domain plus two characters from
the service's id (the same every time).

Tried against a real HestiaCP 1.10.9 (Debian 12, 2026-11-23) through `ferrobill-plugin-cli`:
create (twice), suspend/unsuspend (each twice), change_package (twice), `reset_password` (the new
password works), `panel`, terminate (twice), refusals (a name someone else uses, an unknown
package, no half-made user left behind) and finishing a half-done create - all passed. The same
flows run on every build against a fake one (tests/fake_hestia.rs).

## Setting it up

1. In HestiaCP (as admin): Server settings -> Configure -> enable the API and allow the plugin's
   IP address (or `v-change-sys-config-value API_SYSTEM 1` and
   `v-change-sys-config-value API_ALLOWED_IP <ip>`); then an access key for the admin user with
   no permission list, which gives it the admin's rights (`v-add-access-key admin '' ferrobill
   json`; HestiaCP's permission lists are its named API sets, not single commands, and none of
   them covers making users). Make the packages your products will use. The panel's
   certificate must be one the plugin trusts (a real one, or your CA in the image's trust store).
2. A servers file (keep it secret, for example a Kubernetes Secret mounted as a file):

   ```json
   {"h1": {"url": "https://h1.example.com:8083", "access_key": "...", "secret_key": "...",
           "ip": ""}}
   ```

   `ip` (optional) is the address new domains go on. The names (`h1`) are what ferrobill's
   servers use.
3. Run the image with `FERROBILL_PLUGIN_SECRET`, `SERVERS_FILE` (default
   `/etc/ferrobill-hestiacp/servers.json`) and optionally `LISTEN` (default `0.0.0.0:8080`).
4. In ferrobill: register the plugin and check it, add its servers (Plugins page, Servers) in a
   group, and give each hosting product `{"package": "<HestiaCP package>"}` and that group.
