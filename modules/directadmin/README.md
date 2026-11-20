# DirectAdmin plugin (`ferrobill-directadmin`)

One DirectAdmin user per ferrobill service.

| ferrobill job | DirectAdmin |
|---|---|
| `create` | creates the user (`CMD_API_ACCOUNT_USER`) with the product's `package`, on the configured IP |
| `suspend` / `unsuspend` | suspends or unsuspends the user (`CMD_API_SELECT_USERS`), only if not already so |
| `change_package` | moves the user to the new product's `package` (`CMD_API_MODIFY_USER`) |
| `terminate` | deletes the user and its data (`CMD_API_SELECT_USERS`) |

Actions: `reset_password` (a new password, shown once, never stored by ferrobill) and `panel`
(opens the server's control panel). Single sign-on is planned.

Every job first reads the user (`CMD_API_SHOW_USER_CONFIG`), so a job sent twice does nothing
the second time. The username is the service's `username` field, or is made from the domain plus
two characters from the service's id (the same every time, so a repeated `create` finds its own
account).

> **Not yet tried against a real DirectAdmin**, only a fake one (tests/fake_directadmin.rs).

## Setting it up

1. In DirectAdmin, as the admin or reseller that should own the accounts: Login Keys -> create a
   key allowing `CMD_API_SHOW_USER_CONFIG`, `CMD_API_ACCOUNT_USER`, `CMD_API_SELECT_USERS`,
   `CMD_API_MODIFY_USER` and `CMD_API_USER_PASSWD`, limited to the plugin's IP address. Make the
   user packages your products will use.
2. A servers file (keep it secret, for example a Kubernetes Secret mounted as a file):

   ```json
   {"da1": {"url": "https://da1.example.com:2222", "username": "admin",
            "login_key": "...", "ip": "192.0.2.10"}}
   ```

   `ip` is the address new accounts get. The names (`da1`) are what ferrobill's servers use.
3. Run the image with:

   | Variable | |
   |---|---|
   | `FERROBILL_PLUGIN_SECRET` | from registering the plugin in ferrobill |
   | `SERVERS_FILE` | default `/etc/ferrobill-directadmin/servers.json` |
   | `LISTEN` | default `0.0.0.0:8080` |

4. In ferrobill, register the plugin and run its check, then give each hosting product
   `{"package": "<DirectAdmin package>"}` as its provisioning config. With several servers, also
   name one: `{"package": "basic", "server": "da1"}` (until server groups arrive in ferrobill 0.8).

Try it against a server first with `ferrobill-plugin-cli` (crates/cli): send a `create` job for
a test domain, then `terminate`.
