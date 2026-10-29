# Platform update feeds

Update trust is independent per platform in `packaging/update-trust.json`.
A public key alone does not enable updates: `enabled` stays false until the
release owner approves TEST-key N→N+1 and failure/rollback qualification (Mac
also requires Developer ID signing and notarization). Missing trust on an
enabled platform is an error. The first Mac-capable public release is intended
to be v0.1.1; existing v0.1.0/manual-only installations need one manual upgrade.
They cannot discover this capability retroactively.

The fixed host is `https://updates.oink.hachiwii.com`. Mac owns `mac/`; Windows
owns `windows/`. Client feeds use `v1/mac-2026-01/{stable,beta}/appcast.xml` and
`v1/win-2026-01/{stable,beta}/manifest.json` under those prefixes. Both clients
return `NotConfigured`, `phase: disabled`, and no active checks until enabled.
Preferences and CLI commands cannot bypass this release gate.

## One-time user setup: GitHub Pages and Cloudflare DNS

Use **GitHub Pages**, preserving the existing full-snapshot/CAS/rollback code.
No Cloudflare Worker, account, API token or website deployment is needed. The
website and update host are independent exact hostnames.

1. In `oinkvm/oink` Settings → Pages, choose **GitHub Actions** as the source;
   set the custom domain to **updates.oink.hachiwii.com**. A checked-in CNAME
   does not configure an Actions Pages site's custom domain. Verify ownership
   under the owner's Pages settings using the TXT record GitHub supplies.
2. In the Cloudflare `hachiwii.com` zone add **CNAME** name **updates.oink**,
   target **oinkvm.github.io**, **DNS only** (grey cloud), TTL Auto. No scheme,
   repository path, wildcard or Worker route. Leave the existing website alone.
   Once GitHub's DNS check/certificate completes, enable **Enforce HTTPS**.
3. User creates a fine-grained PAT, resource owner `oinkvm`, selected repository
   `oink` only, **Contents read/write** (Metadata read is implicit). Set it as
   repository Actions secret **UPDATE_FEED_WRITER_TOKEN**. It is not a signing
   key; it only persists the data snapshot through the trusted workflow. A PAT
   cannot itself be scoped to one branch; branch rules and workflow isolation
   provide that boundary. Set an expiry and renew it before expiration.
4. User initializes the data-only branch once (commands below). Protect it from
   force-push and deletion; permit normal fast-forward automation commits. Do
   not require PRs or signed commits on this data branch. Never execute its files.
5. Environments **release**, **macos-update-publishing** and **github-pages**
   must allow the default branch AND **v*** tags: release events have tag refs.
   A main-only environment rule would block publication. Keep any configured
   reviewer approval; approve only after package/update qualification. Pages uses
   `pages:write`/OIDC, while the PAT only commits the data branch.

Run from a reviewed default-branch checkout, with GitHub CLI already authorized:

```sh
state_dir="$(mktemp -d)/feed-state"
python3 build-support/update/feed_site.py init --output "$state_dir"
git -C "$state_dir" init --initial-branch=update-feed-state
git -C "$state_dir" add registry.json site
git -C "$state_dir" commit -m "Initialize disabled update feed state"
git -C "$state_dir" remote add origin https://github.com/oinkvm/oink.git
git -C "$state_dir" push origin HEAD:update-feed-state
```

These are **user** provisioning commands; agents do not run them. Existing
branches are never reset/reinitialized. Initial state contains only a landing
page and CNAME; both feed prefixes return 404 until independently published.
GitHub Pages serves this custom domain from its root, not `/oink/`.
See [GitHub's custom-domain setup](https://docs.github.com/en/pages/configuring-a-custom-domain-for-your-github-pages-site/managing-a-custom-domain-for-your-github-pages-site)
and [Actions publishing](https://docs.github.com/en/pages/getting-started-with-github-pages/configuring-a-publishing-source-for-your-github-pages-site).

### First deployment and HTTPS readiness

A configured CNAME is not a deployment or a certificate. After the reviewed
release code reaches the public default branch, the user runs **Bootstrap
disabled update site** (`bootstrap-update-site.yml`) from that branch. It reads
the existing initial `update-feed-state` snapshot and deploys only its landing
page and CNAME, using `github-pages`/OIDC. It needs no signing keys or writer
PAT, never advances the data branch and refuses any changed/enabled snapshot.
Do not reinitialize an existing state branch. Normal feed deployment deliberately
requires HTTPS readback and cannot be used to bootstrap an unavailable site.

In Settings → Pages, check the DNS validation for `updates.oink.hachiwii.com`
and wait for GitHub's Let's Encrypt certificate to be issued. Then enable
**Enforce HTTPS**. If issuance is still stuck after an hour, the user removes
and re-adds the same custom domain and saves it to restart certificate
provisioning; verify the DNS-only CNAME and any CAA records first (CAA must
permit `letsencrypt.org`). Do not add a second hosting service or disable client
TLS verification. The first deployment does not itself guarantee certificate
issuance; the domain/DNS checks must succeed too.

Before publishing a release, require all of these read-only checks to pass:

```sh
gh api repos/oinkvm/oink/pages --jq '{status,https_enforced,https_certificate}'
curl --fail --show-error https://updates.oink.hachiwii.com/
curl --head https://updates.oink.hachiwii.com/mac/v1/mac-2026-01/stable/appcast.xml
curl --head https://updates.oink.hachiwii.com/windows/v1/win-2026-01/stable/manifest.json
```

The landing page must return 200 with a publicly trusted certificate;
`https_enforced` must be true. Both feed URLs return 404 before the first feed
publication. After the user publishes the v0.1.1 draft, the release controllers
publish **v0.1.1 itself** to both stable feeds, verify the hosted bytes over HTTPS
and commit the resulting snapshot. Existing v0.1.0 installations have updates
disabled and need one manual install of v0.1.1; v0.1.1 then checks the same feed
for later builds. Draft creation never changes the feeds.

See [GitHub's HTTPS provisioning and troubleshooting](https://docs.github.com/en/pages/getting-started-with-github-pages/securing-your-github-pages-site-with-https).

## Mac signing key: user only

On the user's Mac, from the reviewed repository root, run exactly:

```sh
python3 build-support/update/setup-macos-key.py
```

The command downloads/verifies pinned Sparkle 2.9.6, uses its official
`generate_keys --account Oink-mac-2026-01`, retains the private seed in the login
Keychain, and sends it directly in memory to `gh secret set
SPARKLE_ED25519_PRIVATE_KEY --repo oinkvm/oink --env release` via stdin. No
private argv, terminal output, files, logs or evidence. Re-running reuses the
same account; a differing existing public root is refused. GitHub CLI must
already be signed in and able to manage environment secrets. Review/commit only
`packaging/update-trust.json`'s public key; enablement is unchanged. There is no
`SPARKLE_PUBLIC_ED_KEY` secret/variable. The same committed public root goes into
the app's `SUPublicEDKey` and the feed verifier.

Keep a recoverable encrypted backup of the login Keychain. An agent must never
run this command, read/export the resulting production key, or put it in a
qualification build. CI receives the private seed only in the `release`
environment's signing step, through the official signer's `--ed-key-file -`
stdin interface. This replaces the earlier offline-only CI policy by explicit
user decision. The offline `mac-appcast.py` Keychain hook remains available.

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
The pinned Pages action waits for the deployment API's `succeed` status before
readback. Every publication and rollback readback sends `Cache-Control: no-cache`
to revalidate canonical URLs. Pages caches responses for ten minutes; the shared
heartbeat from an immediately preceding platform publication must not be accepted
as the current origin snapshot. Each path is requested once; no sleep or retry
loop substitutes for deployment status or exact byte comparison.
A canceled runner or unavailable Pages service cannot guarantee remote rollback;
the stored parent and previous-feed artifact remain recovery inputs.

## Mac release controller and candidate

`release.yml` workflow_dispatch builds/verifies both platforms and creates a
**draft** containing the notarized DMG, MSI, the ZIP of the already-stapled app,
`release.json` and `SHA256SUMS`. Its initial body comes from
`.github/release-notes.txt`, which is a starting draft. Final update notes must
be approved and committed **before** building the draft. Both platforms embed
the `.github/update-notes.json` language map (all four UI languages) in the
packages. New releases have no
`update.json` asset or notes binding in `release.json`/`SHA256SUMS`; installer
hashes and provenance remain bound exactly as before.

The release procedure is:

1. K1 drafts the English and Simplified Chinese update notes in
   `.github/update-notes.json`. Obtain user approval, translate Japanese and
   Korean, validate and commit all four languages on the trusted default branch.
   Use `channel: stable`, `severity: normal|critical`; each newline separates a
   plain-text bullet. Include user-visible changes only. The unsigned-MSI
   limitation belongs in the GitHub release's Known limitations.

   ```sh
   PYTHONPATH=build-support/update python3 -c 'import published_release; published_release.update_notes()'
   git add .github/update-notes.json
   git commit -m "docs(release): finalize update notes"
   ```

2. Dispatch **Release packages** on the default branch (currently `main`), using
   the reserved monotonically increasing build. For example, for 0.2.0/build 35:

   ```sh
   gh workflow run release.yml --repo oinkvm/oink --ref main -f build=35
   ```

3. The user may edit the draft's release body directly on GitHub, reviews the
   draft and clicks **Publish**. Publishing the GitHub release triggers no
   workflow and does not publish either update feed. GitHub body edits, including
   later edits, do not change the embedded/committed in-app notes.

4. Keep the approved file unchanged for this build. After publication, explicitly
   dispatch **Seed published stable release feeds** on the trusted default branch
   carrying those same finalized notes, for the published tag:

   ```sh
   RELEASE_TAG=v0.2.0 # replace with the published release tag
   gh workflow run seed-update-feeds.yml --repo oinkvm/oink --ref main \
     -f release_tag="$RELEASE_TAG" -f platform=both
   ```

The manual dispatcher and both reusable publishers check out the dispatch's
exact default-branch SHA, so code and notes remain consistent even if the branch
advances during the run. They validate notes with `feed.validate_notes` and the
stable-channel/severity rules before signing. Windows reads notes from that
trusted checkout at signing time; Mac embeds them in the signed appcast.
Neither publisher downloads release-asset notes, including legacy `update.json`.
Do not change the notes between retries for a build already registered in the
Windows catalog: the immutable-build check still rejects changed records.

Notes are an extensible language map on the existing feeds and signing schema.
Clients require English; the publisher requires English and Simplified Chinese,
at most 32 case-insensitively unique tags matching
`[A-Za-z]{2,3}(-[A-Za-z0-9]{2,8})*`, and existing 16 KiB/control-character limits
per translation. Lookup uses exact UI language, progressively shorter parents,
then English. 0.1.3 publishes only the approved `en` and `zh-Hans` entries so
strict 0.1.1/0.1.2 clients can reach it. From 0.1.4 onward, adding languages is
an accepted hard cut: those older clients reject the feed. No new URLs, v2
schema, dual writing or compatibility projection is introduced. Notes are bound
into signed appcasts/manifests and catalog records at manual feed publication;
ZIP/DMG/MSI hashes remain bound into the release record and checksums. Existing
signed feeds and catalog records remain valid. Windows manifest renewal uses
those existing records and never reads the current `.github/update-notes.json`.
Private symbols are never release assets.

The manual dispatch is the single release feed publication path for both
platforms. Pages can report success without serving a new artifact when several
deployments run under a release event; see
[actions/deploy-pages#383](https://github.com/actions/deploy-pages/issues/383).
Using the supported dispatch event avoids that behavior without deployment-ID
changes, delays or retries. No `release.published` trigger or automatic dispatch
remains. Drafts and prereleases are rejected by the live-release gate.

To recover a failed initial publication without rebuilding
or retagging the release, dispatch **Seed published stable release feeds** on the
default branch with `release_tag` and `platform=mac`, `windows` or `both` (default).
Select only the platform that has not committed, preserving the other platform's
catalog and sequence. The shared authorization
checks the live GitHub API for a published, non-draft, non-prerelease stable
release before either publisher proceeds. Both paths use the same native/source/
hash/signature checks, monotonic catalog and transactional Pages publisher.
An already-committed platform build remains protected against republication.
Disabled trust skips before accessing secrets or the
data branch. It runs trusted default-branch code, fetches the tag as data,
requires it to be an ancestor of that branch, and binds the release version,
build, built commit, source-tree digest, archive bytes and reviewed language-map notes.
It never executes tag/data-branch code. Native Developer ID/team/ticket and the
recorded 10-second signed smoke/notarization are checked before signing.
The official pinned `sign_update` signs the exact ZIP and then the final appcast;
both are independently verified with the committed public key. No delta builds.
Old stable items are retained. Stable releases also reach beta subscribers;
a newer beta prefix is retained byte-for-byte with its prior qualification. The signed
candidate is a **same-run artifact** passed to the existing verifier/deployer;
no artifact from a PR or unrelated run can select update bytes.

The artifact contains `site/mac/v1/mac-2026-01/stable/appcast.xml` and
`catalog.json`, keyed by the complete relative path. Catalogs bind arm64, build,
version, URL, length, SHA-256, native identity/notarization and qualified build.
Reviewed trust enablement records that the product's TEST-key qualification has
passed; strings in an arbitrary download do not establish qualification. The
publisher verifies public package bytes again and refuses non-increasing builds.
A failed publication never silently replaces the prior feed; inspect the Pages
rollback result before rerunning. Re-publishing an already committed build is
rejected; no force-push or feed downgrade is a rollback mechanism.

Signing keys stay in the **release** environment. Every reusable-workflow caller
must explicitly forward `SPARKLE_ED25519_PRIVATE_KEY` or
`WINDOWS_UPDATE_ED25519_PEM_WIN_2026_01` by name, even though the key exists only
in that environment. GitHub supplies the environment value to the sign job;
an omitted name otherwise resolves empty. The reusable interfaces require the
names, packaging tests check every caller, and signers reject empty values before
invoking cryptographic tools. No private key belongs in repository secrets or
source files for this purpose.

## Settings and qualification

Default policy is **stable / automatic checks / notify**. Notify is the only
production behavior, not a persisted mode: preferences contain checks_enabled
and channel only. Checking never downloads or installs a candidate. The user
explicitly downloads it, then clicks Install when its typed phase is Ready.
Manual Check is never rate-limited: concurrent requests share the queued or
active check and its status/events result; a completed check can be refreshed
immediately. Only background checks use the daily scheduling interval.
Install immediately interrupts input, audio (including silent open streams),
drag/promises, clipboard/file transfer and other sharing work; it never waits
for an idle interval, session unlock or activity completion. The backend closes
new-work admission, releases held input through an injection fence, restores
source mute while suspending audio, and temporarily suspends its provider before
replacement/relaunch. Failed cleanup/integration or another installer produces
a bounded explicit failure; no hidden retry or WaitingForIdle continuation.
Aborted preparation reopens admission and reconciles audio without changing the
user's forwarding preferences or exit selection.

There is **no production automatic-install setting, IPC command or CLI option**.
Qualification automation belongs only to the external TEST harness, which polls
normal status and explicitly sends Check, Download and Install(expected_build)
against the installed TEST build. It uses the same immediate Install path as a
click, without synthetic input, an automatic-mode preference or a safety bypass.
Stored policies with unknown fields (including a removed mode) are invalid under
the existing strict parser; there is no migration for unreleased TEST state.

Check-for-updates uses native update IPC when enabled; disabled versions open
Releases. Mac settings are per-user; Windows settings are machine-wide and normal
users see why administrator permission is needed. The package pipeline rejects
the removed idle-install phase/policy in both executable and frontend content.

Before enabling Mac trust, use **TEST** Ed25519 material and an external test
source export with a compiled loopback feed, never the production root or feed.
Sparkle 2.9.6 permits EdDSA-signed HTTP on local networks (SPUUpdater's security
check explicitly warns rather than rejects). Qualification alone may add
`NSAllowsLocalNetworking` in its TEST plist. Production content checks forbid
ATS exceptions; no runtime URL/key override is added. Get approval of the real
Mac/path first. An install owner installs test N; K1 drives the actual custom
Sparkle bridge via IPC, verifies N→N+1 download/signature/interruption/fence/swap/relaunch,
and verifies a rejected/corrupt update leaves the previous app usable, provider
state connected, and input/admission restored. A rejection before replacement
is distinct from post-install health rollback: Sparkle does not implement the
Windows health journal. Record exactly which failure stage was qualified.
Only the install owner restores the user's accepted build. No input injection,
CA/root-trust changes or production endpoint publication. Stop the test server,
unregister only test/staging bundles, and remove test archives/cache afterward.
Never label pure tests or packaging smoke as real Sparkle qualification.

## Windows candidate

The Windows reusable publisher takes `release_tag`, fetches the final published
`release.json` and MSI, and binds them to the fetched tag's commit and exact
source-tree digest using trusted default-branch code. The signing job reads
validated notes from `.github/update-notes.json` in the dispatch's pinned
default-branch checkout, independently of the package assets. Native MSI
inspection checks the embedded inventory. Ed25519 metadata/package signatures
are mandatory; Authenticode/SignPath pins are additionally enforced when
configured. An empty reviewed pin list permits unsigned Windows packages.
No arbitrary path/URL/key/command line can enter the SYSTEM updater through IPC.

Windows package URLs in a verified manifest may use **any HTTPS host**, including
R2 or another CDN; neither repository release paths nor redirect hosts are pinned
in installed clients. Every redirect hop must remain HTTPS, contain no userinfo
or credentials, and stay within five redirects. Query strings, object paths and
HTTPS ports are allowed. The manifest itself remains on the fixed update host.
The authority to install comes from the pinned Ed25519-signed manifest and the
downloaded package's exact SHA-256 and size (plus its Ed25519 signature), checked
before staging/use; HTTPS or a package host is never an independent trust root.
A compromised package host can deny service but cannot substitute accepted bytes
without the signing key. Optional Authenticode pins still add their own check.
The Mac controller likewise delegates enclosure downloads to Sparkle without a
package-host allowlist; its signed feed/archive checks remain authoritative.

Windows treats administrators and SYSTEM as trusted. Ordinary users cannot write
the maintenance directory or its records. Package verification occurs once after
download, and MSI/cabinet inspection once before staging the protected plan. The
worker checks manifest freshness immediately before MSI, holds the staged MSI
read-only during use, and uses one product/hash/removal authorization record with
the effective SYSTEM caller and maintenance lock. There are no worker identity,
journal-hash or ancestor-ACL bindings, duplicated lock handles or guardian leases.
MSI owns native rollback; the worker owns post-install health, rollback and its
recovery task. See [the admission contract](../../docs/release/windows-msi-admission.md).

The user-only command is `python3 build-support/update/setup-windows-key.py`.
Its `WINDOWS_UPDATE_ED25519_PEM_WIN_2026_01` secret belongs to the **release**
environment; only the public root is committed and enablement remains off until
qualification approval. The publisher uses the existing protected stage/worker,
signed records and sequence CAS. Registration verifies public immutable asset
bytes again. Receipts use `source_tree_sha256`; signed manifests use
`sourceTreeSha256`. Both bind the same 64 lowercase hexadecimal digest.
Authenticode roots, when configured, are SHA-256 of SubjectPublicKeyInfo, never
certificate holder names. Production signing material is never used for tests.

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
