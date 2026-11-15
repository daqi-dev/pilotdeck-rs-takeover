# Stock browser approver session

The stock shell provides a StaffDeck account action when the approval inbox module
is present. Sign in with the normal StaffDeck username and password. The host uses
the fixed `STAFFDECK_FORMAL_API_ORIGIN`, `STAFFDECK_COPY_TENANT_ID`,
`STAFFDECK_APPROVAL_USER_ID`, and `STAFFDECK_COPY_PILOTDECK_USER_ID` binding.
The PD authenticated transport forwards normal login to `/api/auth/login` and
validates the returned bearer with `/api/auth/me`, including the configured user
and tenant. It never substitutes a management credential or PD login token.

The normal StaffDeck bearer stays in browser memory. Logout, PD identity change,
or shell unmount clears it; reload requires normal sign-in again. Window focus
revalidates the normal session. Login/logout/revalidation refresh and cancel inbox
requests. Durable waits and receipts remain server-owned and are read again after
sign-in using the original session, wait, revision and authority rules.

The SOP discovery profile supports an exact `${ENVIRONMENT_VARIABLE}` API-key
reference using the existing credential resolver. Deployment helpers write only
the reference; the host process supplies its value. Missing references fail
configuration validation rather than sending the reference as a bearer.
