# Platform update feeds

Beta.1 is manual-only on both platforms. `packaging/update-trust.json` contains
independent `mac.enabled` and `windows[].enabled` gates, all false, with null keys
and no Authenticode pins. A configured key alone does not enable updates. A
platform's release owner enables it only after its signed N→N+1 and rollback
qualification (Mac also requires notarization; Windows requires SignPath).
Missing trust on an enabled platform is an error, never a disabled fallback.

The fixed host is `https://updates.oink.hachiwii.com`. Mac owns `mac/`; Windows
owns `windows/`. Client feeds use `v1/mac-2026-01/{stable,beta}/appcast.xml` and
`v1/win-2026-01/{stable,beta}/manifest.json` under those prefixes. Both clients
return `NotConfigured`, `phase: disabled`, and no active checks until enabled.
Preferences and CLI commands cannot bypass this release gate.

## State and transaction

Protect `update-feed-state` against force-push and restrict writes to the feed
writer identity (`UPDATE_FEED_WRITER_TOKEN`). The branch is data only. Initialize
`registry.json` with explicit disabled platforms, revision 0 and Windows slots:

```json
{
  "schema": 1,
  "revision": 0,
  "platforms": {
    "mac": {"enabled": false, "revision": 0, "files": {}},
    "windows": {"enabled": false, "revision": 0, "files": {}}
  },
  "feeds": {
    "win-2026-01": {
      "stable": {"records": [], "last_allocated": 0, "manifest_sha256": null},
      "beta": {"records": [], "last_allocated": 0, "manifest_sha256": null}
    }
  }
}
```

Start `site/` with `CNAME` containing `updates.oink.hachiwii.com` and a static
`index.html` landing page; it has no platform feeds. `records/` holds public
content-addressed Windows release records. Configure the Pages custom domain,
DNS and HTTPS separately; CNAME alone does not configure the service.

`publish-macos-feed.yml` and `publish-windows-feed.yml` are independent reusable
workflows called from trusted default-branch release controllers. They consume
artifacts from the same run. Build/signing/provenance verification and real
qualification precede those calls; strings in a downloaded record are not proof
of qualification. Publishers have separate protected environments. The Mac job
never accesses a Windows epoch or signing secret.

`renew-windows-update-manifests.yml` runs daily at 03:17 UTC. Disabled Windows
catalogs skip before entering the signing environment. An enabled catalog with
missing trust fails. Renewal may only extend expiry and sequence of the unchanged
Windows catalog. It never creates a Mac feed. Windows TTL remains 30 days, with
renewal at 21 days remaining. Daily successful readback refreshes the host
heartbeat even when the signed Windows bytes do not need renewal. Mac has no
Windows TTL or sequence dependency.

Every state writer uses concurrency group `oink-update-feed-publish`. Windows
reserves sequences and commits metadata before signing, so a failed allocation
is not reused. Signed candidates carry the platform revision they observed.
`deploy-feed.yml` takes the lock, reads the latest complete snapshot, verifies
signatures and packages, and replaces only the nominated prefix. The other
platform's bytes survive exactly; absent prefixes remain absent. Deletion is not
part of this API and requires a separate reviewed operation. Older candidates,
changed reservations, and non-increasing builds/sequences are rejected.

Deployment contains no signing environment or key. It uploads the old snapshot
for rollback, deploys the merged snapshot, reads back exact bytes on the public
host (and checks absent platforms return 404), then commits state with a normal
fast-forward Git push as CAS. Deployment, readback or state-CAS failures redeploy
the frozen prior snapshot and verify its bytes. Rollback failure is reported as
failure; it must be repaired from that prior snapshot before another publication.
A canceled runner or unavailable Pages service cannot guarantee remote rollback;
the stored parent and previous-feed artifact remain recovery inputs.

## Mac candidate

The offline signer uses only an existing Keychain account, default
`Oink-mac-2026-01`, and the pinned Sparkle 2.9.6 distribution. It never creates a
key. Run `mac-appcast.py` on approved arm64 ZIP/DMG archives with `--build`, `--tag`,
`--notes` (required `en` and `zh-Hans` plain-text JSON), and `--channel`. It embeds
bilingual notes before the final official Sparkle signature. Do not modify the
signed bytes afterwards. No delta generation is enabled.

The publisher artifact contains `site/mac/v1/mac-2026-01/<channel>/appcast.xml`
and `catalog.json`, keyed by that complete relative path. Each catalog entry has
`architecture: "arm64"`, integer `build`, `version`, `url`, integer `length`,
`sha256`, `developer_id_verified: true`, `notarized: true`, and `qualified_build`
equal to the build. The release controller supplies that build separately as
`qualified_build`. The public verifier checks the signed XML, package signature,
length/hash and catalog match; the protected qualification job supplies native
Developer ID/notarization and N→N+1 evidence. Pass a full Mac prefix including
any unchanged channel; publication does not implicitly delete channels.

## Windows candidate

The publisher takes the final SignPath-verified MSI, `release.json` and
`update.json` (channel, severity, bilingual notes), plus source tree SHA-256,
release tag, SignPath approval and qualified build. Receipt provenance is
`source_tree_sha256` (64 lowercase hex); the signed manifest uses
`sourceTreeSha256`. Public package signing checks exact artifact hash/length and
provenance. Registration verifies immutable GitHub Release asset bytes again.
The update signing secret is `WINDOWS_UPDATE_ED25519_PEM_WIN_2026_01`, restricted
to the Windows signing environment. It is written only to a temporary 0600 file,
never argv, workflow outputs or public artifacts. Authenticode roots are SHA-256
of signer SubjectPublicKeyInfo, not certificate names.

The final package/native signature verifier belongs to the release pipeline.
Both its successful result and this run's qualification result must be bound to
the exact package before invoking the publisher; no publisher approves SignPath
or performs an install. Source code is always the trusted workflow checkout.

## Verification

Run `python3 -m unittest discover -s packaging/tests -p 'test_*feed*.py'` and
`cargo test -p oink-update`. `test_mac_appcast.py` additionally tests bilingual
Sparkle XML and the pinned native signer when `SPARKLE_TEST_SIGN_UPDATE` is set.
All signing fixtures use public RFC 8032 test material, never production keys.

`feed.py monitor` is the Windows expiry/sequence monitor. Run it independently
of Actions with persistent seen-sequence state; 14/7-day expiry and 48-hour host
heartbeat thresholds alert the release owner. A disabled Windows platform is
reported as skipped. `feed_site.py verify-public --state <snapshot>` verifies
both enabled platforms and absent prefixes. Pass `--platform mac` or
`--platform windows` for separate platform monitoring. A host heartbeat does not imply
Windows availability, and Mac publication does not renew Windows metadata.
Real signed upgrades, rollback, permissions and CDN readback remain release
qualification; local pure tests do not claim those actions occurred.
