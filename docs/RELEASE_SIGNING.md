# Release signing and notarization

Clew keeps **build/package** and **release signing** as two separate trust boundaries.

`cargo xtask package` remains the default path. It produces a deterministic **unsigned schema-2** artifact and never probes for, selects, or uses a signing identity. Signing is an explicit second step:

```text
clean schema-2 unsigned artifact
        ↓ verify sidecar + archive + every payload hash
private temporary staging copy
        ↓ platform signing / notarization
signed schema-3 artifact
        ↓ re-hash every final payload file + OS verification
.release.json + SHA256SUMS
```

The signing step never modifies `target/<triple>/<profile>/clew[.exe]` and never edits the input unsigned ZIP. A failed signing attempt must leave both untouched and must not publish a partial signed artifact.

## 1. What may and may not enter Clew

Safe public selectors accepted by `cargo xtask sign-package`:

- Windows certificate SHA-1 thumbprint;
- Windows RFC3161 timestamp service URL;
- macOS Developer ID Application identity string;
- macOS `notarytool` Keychain profile **name**;
- optional path to an installed `signtool.exe`.

Secrets stay in operating-system credential stores. Do **not** put any of these in the repository, command history, release manifest, CI YAML, or Clew arguments:

- PFX/P12 password;
- private-key bytes;
- Apple ID password or app-specific password;
- App Store Connect private-key contents;
- Keychain unlock password.

Clew deliberately has no `--pfx-password`, `--apple-password`, or equivalent option. Import credentials using the platform's normal secure tooling first, then pass only the public selector/profile name to Clew.

## 2. Input contract

`sign-package` accepts only an artifact whose sidecar proves all of the following:

- `schema_version = 2`;
- `product = "clew"` and `app_id = "io.clew.app"`;
- `dirty = false`;
- `unsigned = true` and no existing `signing` record;
- platform-native V6b-2 layout;
- bounded, unique, safe relative payload paths;
- the ZIP filename, size, SHA-256, embedded manifest, and every payload SHA-256 match the sidecar.

Windows artifacts must be signed on Windows; macOS artifacts must be signed on macOS. Linux release signing is not defined by V6b-3 and is rejected rather than silently inventing a policy.

## 3. Windows Authenticode

### 3.1 Prepare the signing identity

Install a code-signing certificate **with its private key** into an appropriate Windows certificate store using normal Windows/CA procedures. Record its 40-hex SHA-1 thumbprint; the thumbprint is an identifier, not the private key.

By default Clew asks SignTool to use the current-user store. Use `--machine-store` only when the certificate was intentionally installed in the machine store and the release runner has access to its private key.

Clew resolves SignTool in this order:

1. explicit `--signtool <path>`;
2. `signtool.exe` on `PATH`;
3. the newest matching architecture under the standard Windows Kits 10 SDK `bin` tree.

### 3.2 Sign

```text
cargo xtask sign-package \
  --manifest dist/clew-v0.1.0-x86_64-pc-windows-msvc.release.json \
  --out-dir dist-signed \
  windows \
  --cert-sha1 <40-HEX-THUMBPRINT> \
  --timestamp-url <RFC3161-HTTP-OR-HTTPS-URL>
```

The generated SignTool operation always sets:

```text
/fd SHA256
/tr <RFC3161 URL>
/td SHA256
```

Clew then independently runs SignTool verification with the Windows Authenticode policy before it writes the final signed release sidecar.

### 3.3 Manual verification

After extracting the signed ZIP, a release operator can independently run:

```text
signtool verify /pa /all /v clew.exe
```

The final signed sidecar is schema 3, has `unsigned=false`, records `mechanism=windows-authenticode` and the public certificate thumbprint, and re-hashes the post-signing executable bytes.

## 4. macOS Developer ID + notarization

### 4.1 Prepare credentials outside the repository

The release Mac needs:

- a valid **Developer ID Application** identity in its Keychain;
- Xcode/Command Line Tools providing `codesign`, `notarytool`, and `stapler`;
- an Apple-supported `notarytool` Keychain profile created outside the repository (for example with `xcrun notarytool store-credentials`).

Store Apple authentication material in Keychain. Clew receives only the Developer ID identity string and Keychain profile name.

### 4.2 Sign and notarize

```text
cargo xtask sign-package \
  --manifest dist/clew-v0.1.0-x86_64-apple-darwin.release.json \
  --out-dir dist-signed \
  macos \
  --identity "Developer ID Application: Example Organization (TEAMID)" \
  --notary-profile clew-notary
```

Clew performs the release operation inside-out:

```text
sign Contents/Resources/clew
  -> verify nested CLI
  -> sign Clew.app with secure timestamp + Hardened Runtime
  -> verify Clew.app
  -> submit an app ZIP with notarytool and wait for Accepted
  -> staple Clew.app
  -> stapler validate
  -> spctl assess
  -> package the stapled app with ditto
  -> extract the final ZIP and repeat signature/staple/Gatekeeper verification
```

The old `altool` notarization path is intentionally not supported.

For the current V6b-2 bundle, codesign is allowed to add exactly:

```text
Clew.app/Contents/_CodeSignature/CodeResources
```

Any other unexpected regular file or any symlink introduced into the signed staging tree fails the release rather than being silently shipped.

### 4.3 Manual verification

After extracting the signed ZIP:

```text
codesign --verify --strict --verbose=2 Clew.app
xcrun stapler validate Clew.app
spctl --assess --type exec --verbose=4 Clew.app
```

The schema-3 manifest records the public Developer ID identity and notarization submission ID, plus `timestamped=true`, `notarized=true`, and `stapled=true` only after all corresponding commands have succeeded.

## 5. Signed artifacts are auditable, not byte-reproducible

Unsigned schema-2 artifacts are the reproducible build baseline. Signed artifacts intentionally include external cryptographic/time services:

- Authenticode secure/RFC3161 timestamps;
- Developer ID secure timestamps;
- Apple notarization tickets.

Therefore two legitimate signed builds are not required to have the same ZIP SHA-256. Instead each schema-3 release records the final post-signing payload hashes and final archive hash, and the release gate additionally requires native OS signature verification.

Never compare a signed artifact against the unsigned file hashes and call the difference corruption: signing necessarily changes executable/bundle bytes.

## 6. CI boundary

The normal cross-platform CI job remains unsigned and secret-free. It continues to run:

```text
cargo xtask package --out-dir dist
```

A future protected release job may call `sign-package` only on trusted runners where the platform credential store has already been provisioned. Repository pull requests and ordinary CI must remain able to build and test Clew without signing credentials.

If a credential is absent, `sign-package` must fail closed. It must never fall back to ad-hoc signing, a self-signed certificate, or `unsigned=false` without a successful native signing/notarization verification chain.
