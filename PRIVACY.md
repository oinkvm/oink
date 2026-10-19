# Oink privacy notice

Crash and performance reporting is **off by default**. If you opt in, Oink sends
limited reports to **Sentry, our processor, in its EU data region** to help diagnose
faults and improve performance. Reports may be queued and sent later, including
after restarting Oink. Builds without reporting configured send no reports.

- **Crash reports:** app version/build, broad OS version, affected Oink component,
  crash category and optional numeric exception code, and available code locations
  (shipped-build debug IDs and instruction offsets, or numeric source locations).
- **Performance reports:** sampled durations and timing histograms for input,
  connections, drag and drop, and audio, with fixed categories such as platform,
  direct/relay connection, transport, and codec, plus app/build and broad OS version.
- Both use fresh random report identifiers and an occurrence time rounded to the
  hour; performance reports also include measured durations. There is no persistent
  installation or device identifier.

These reports never include screen, clipboard, keyboard/mouse, audio, or file
content; file paths, user/device/host names; logs or breadcrumbs; error-message
text; or raw crash dumps, memory, or local variables. Raw crash data is processed
locally into the limited fields above. Oink includes no IP address in the payload
and explicitly disables Sentry's IP inference. Sentry still sees the connection's
IP address when receiving HTTPS traffic.

Reports are retained **up to 90 days, per the Sentry project's retention**. Local
diagnostics stay on your devices; Oink can gather diagnostics from paired devices
into a local bundle. Bundles are uploaded to us only when you choose to share them;
enabling reports does not upload them.

Turn reporting off at **Settings → Diagnostics → Send crash and performance reports**.
This stops collection and sending and clears pending local reports. It cannot
recall reports already sent.

At startup, Oink downloads the sponsor QR image from oink.hachiwii.com; this request contains no personal data beyond the IP address any web request reveals.

Contact: [legal@hachiwii.com](mailto:legal@hachiwii.com).
