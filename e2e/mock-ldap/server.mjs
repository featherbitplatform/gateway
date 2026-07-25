/**
 * A minimal mock LDAP server for the e2e suite (ldap-auth).
 *
 * ldap-auth reads HTTP Basic credentials, builds a bind DN
 * `<uid>=<username>,<base_dn>`, and does a real LDAP simple bind against this
 * server. So unlike the HTTP mocks, this has to speak the actual LDAP protocol
 * on a TCP socket -- ldapjs provides it in pure JS (no container, no OpenLDAP).
 *
 * Accepts a simple bind for cn=alice,ou=users,dc=example,dc=org with password
 * "secret"; every other DN or password is rejected with invalidCredentials.
 *
 * Port 3389 (LDAP_PORT overrides). Not 389 -- that needs root.
 */
import ldap from 'ldapjs';

const PORT = Number(process.env.LDAP_PORT ?? 3389);
const VALID_DN = 'cn=alice,ou=users,dc=example,dc=org';
const VALID_PASSWORD = 'secret';

const server = ldap.createServer();

// A bare bind route under the base DN. ldap-auth binds directly as the user DN
// (no search), so we only need to authenticate the bind.
server.bind('dc=example,dc=org', (req, res, next) => {
  const dn = req.dn.toString().toLowerCase().replace(/,\s+/g, ',');
  const password = req.credentials;

  if (dn === VALID_DN && password === VALID_PASSWORD) {
    res.end(); // success
    return next();
  }
  return next(new ldap.InvalidCredentialsError());
});

server.listen(PORT, '127.0.0.1', () => {
  console.log(`[mock-ldap] listening on ldap://127.0.0.1:${PORT}`);
});
