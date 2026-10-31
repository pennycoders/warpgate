Fork of `ironrdp-connector` 0.12.0.

## Credential-less CredSSP (interactive logon)

Upstream hardcodes `CredSspMode::WithCredentials`, so every NLA connection delegates
`TSPasswordCreds` and the server auto-logs the session on with them. Warpgate's
interactive-logon target option needs the opposite: authenticate the connection over
CredSSP but delegate nothing, leaving the server to present its own sign-in screen.

The fork adds `Config::credssp_credentialless` and `CredsspSequence::init_with_mode`, a
copy of `init` with a trailing `credentialless: bool` that selects
`CredSspMode::CredentialLess` (sspi already implements it — empty `TSCredentials` in the
final TSRequest). `init` keeps its signature and delegates with `false`. The only caller
that needs the knob is `perform_credssp_step` in `ironrdp-async`, which is vendored
alongside (see `vendor/ironrdp-async/PATCHES.md`) solely to pass it.

Worth offering upstream as a `Config` field; drop both forks once a release carries an
equivalent knob.

## sspi 0.23.0 and picky 7.0.0-rc.26

0.12.0 pins `picky = "=7.0.0-rc.26"` and builds `sspi::KerberosConfig` with its `kdc_url`
field. sspi 0.23.1 needs `picky = "=7.0.0-rc.27"` and replaces that field with
`kdc_resolution` (IronRDP master follows it in #1987), so the dependency tree stays on sspi
0.23.0 and picky rc.26 until an `ironrdp-connector` release moves to them; this fork does not
carry that port.

`Cargo.toml` is the published one plus `warnings = { level = "allow", priority = 1 }`
under `[lints.rust]`, so this vendored path dependency's warnings don't surface in
Warpgate's builds. That is not in `warpgate.patch` (which is source-only); re-apply it by
hand on re-vendor.
