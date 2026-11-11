# Windows auto unlock

`OinkCredentialProvider.dll` is a native Credential Provider V2. It is built by
`build-support/windows_credential_provider.py`, shipped in the MSI File table,
and signed independently. It is not a sixth executable in `Receipt.components`.
The SYSTEM service registers only this provider's CLSID; it never registers a
filter, wraps a system provider, or disables Windows sign-in methods.

The Rust API is exported from `oink-winsvc`. The service's native component owns
all password and operation state. Save validates the password with a network
logon and does not create an interactive session. It uses
SYSTEM user-scope DPAPI, with SYSTEM-owned, SYSTEM-only files in a pinned private
ProgramData directory. Disable cancels pending work and deletes the ciphertext
and status. Standalone uninstall invokes the protected service image to remove
that private directory and unregister the provider. Upgrade preserves enrollment.
The uninstall step runs as SYSTEM in the msiexec client's session (not session 0),
treats an absent directory or registration as removed, and fails with the step
and object that failed; the MSI action copies that line into the installer log.

An idle service listener blocks in overlapped ConnectNamedPipe, waking only for
connection, session change or stop. Request I/O is bounded and cancellation is
drained before releasing buffers. With no logged-on console user, the worker
waits on session/stop events. The provider retains the probe's 250 ms authenticated
polling interval while advised; this is a known performance boundary, not an
idle-polling loop in the SYSTEM service.

The named pipe rejects remote clients and competing instances. Before sending a
password the client binds its connected pipe to the SYSTEM-owned object and the
running OinkInput SCM PID. The service reuses the existing protected backend
image/SID/session checks and identification impersonation. Acquire and Outcome
accept only the current console's SYSTEM LogonUI. No credential command takes a
path, account name, environment, network payload, or password on the command line.

Attempt acknowledges an enqueued operation. The service injects no click, key, or
cursor move into LockApp; the user dismisses the curtain and enters the LogonUI
credential page. The provider supports both CPUS_LOGON and CPUS_UNLOCK_WORKSTATION.
Its first authenticated Poll from GetCredentialCount is the readiness edge. The
service then allows the one-shot operation only when a numeric source tag, already
confirmed unlocked by the agent's UnlockReply, reported remote input in the prior
two monotonic seconds. A rejected edge exposes zero Oink credentials and cannot be
made eligible by a later input. CredentialsChanged/GetCredentialCount(auto)/
GetSerialization still polls the service ledger across enumeration and instance
reconstruction.

Pending expires after 30 seconds. Acquire consumes before decrypting and flushes
a durable failure latch before returning any secret. A failed or uncertain send
cannot be retried. Last result survives restart; interrupted Pending becomes
Expired, interrupted Submitted becomes Failed. Only Save/Disable clears a failure.
WTS success is correlated with the operation and session; neither own nor unknown
session events produce a network announcement. Service restart abandons pending
operations; the DLL can connect to the new authenticated service, but cannot
replay an old operation ID. Windows owns the serialized credential buffer; all
other owned plaintext buffers are wiped.

Native policy tests exercise operation state without Windows authentication.
Building this component is not evidence of a successful real unlock, DPAPI ACL
isolation or MSI install/upgrade. Those require separate authorized Windows
acceptance. Passwordless accounts and non-local identity providers have not been
validated. MSI may require a reboot to replace a DLL still loaded in LogonUI;
the product does not terminate LogonUI to force replacement.
