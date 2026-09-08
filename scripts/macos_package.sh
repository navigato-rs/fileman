#!/usr/bin/env bash
# Sign, notarize, and package the macOS app bundle into dist/.
#
# Produces two artifacts from target/.../bundle/osx/FileMan.app:
#   dist/fileman-macos-aarch64.zip  portable archive
#   dist/fileman-macos-aarch64.dmg  drag-to-Applications installer
#
# Signing is driven entirely by environment variables (see CONTRIBUTING.md):
#
#   MACOS_CERTIFICATE      base64 of the Developer ID Application .p12
#   MACOS_CERTIFICATE_PWD  password that .p12 was exported with
#   MACOS_SIGN_IDENTITY    optional; auto-detected from the keychain if unset
#
# plus notary credentials, either an App Store Connect API key:
#
#   APPLE_API_KEY          base64 of the AuthKey_XXXX.p8
#   APPLE_API_KEY_ID       the key ID
#   APPLE_API_ISSUER_ID    the issuer UUID
#
# or an Apple ID:
#
#   APPLE_ID               developer account e-mail
#   APPLE_APP_PASSWORD     app-specific password
#   APPLE_TEAM_ID          10-character team ID
#
# With MACOS_CERTIFICATE unset the script falls back to an ad-hoc signature so
# forks still get artifacts. Those builds are NOT notarized: Gatekeeper refuses
# to open them until the user clears the quarantine flag by hand.
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
app="${root_dir}/target/aarch64-apple-darwin/release/bundle/osx/FileMan.app"
dist_dir="${root_dir}/dist"
zip_path="${dist_dir}/fileman-macos-aarch64.zip"
dmg_path="${dist_dir}/fileman-macos-aarch64.dmg"

work_dir="$(mktemp -d)"
keychain=""

cleanup() {
  if [[ -n "${keychain}" ]]; then
    security delete-keychain "${keychain}" 2>/dev/null || true
  fi
  rm -rf "${work_dir}"
}
trap cleanup EXIT

if [[ ! -d "${app}" ]]; then
  echo "Missing app bundle at ${app}" >&2
  exit 1
fi
mkdir -p "${dist_dir}"

# --- Import the Developer ID certificate into a throwaway keychain ---

identity="-"
if [[ -n "${MACOS_CERTIFICATE:-}" ]]; then
  keychain="${work_dir}/fileman-signing.keychain-db"
  keychain_pwd="$(openssl rand -base64 24)"
  cert_file="${work_dir}/certificate.p12"

  printf '%s' "${MACOS_CERTIFICATE}" | base64 --decode > "${cert_file}"
  security create-keychain -p "${keychain_pwd}" "${keychain}"
  # Disable the lock-on-sleep timeout; notarization can outlive the default.
  security set-keychain-settings -lut 21600 "${keychain}"
  security unlock-keychain -p "${keychain_pwd}" "${keychain}"
  security import "${cert_file}" -k "${keychain}" -P "${MACOS_CERTIFICATE_PWD:-}" \
    -T /usr/bin/codesign -T /usr/bin/security
  # Without this codesign triggers an interactive "allow access?" prompt that
  # nothing can answer on a CI runner, and the build hangs until it times out.
  security set-key-partition-list -S apple-tool:,apple:,codesign: \
    -s -k "${keychain_pwd}" "${keychain}" > /dev/null
  security list-keychains -d user -s "${keychain}" "$(security default-keychain | tr -d ' "')"

  if [[ -n "${MACOS_SIGN_IDENTITY:-}" ]]; then
    identity="${MACOS_SIGN_IDENTITY}"
  else
    # Prefer the certificate hash over the common name: it is unambiguous even
    # when several Developer ID certificates share a subject.
    identity="$(security find-identity -v -p codesigning "${keychain}" \
      | awk '/Developer ID Application/ { print $2; exit }')"
    if [[ -z "${identity}" ]]; then
      echo "No 'Developer ID Application' identity in the imported certificate" >&2
      exit 1
    fi
  fi
fi

# --- Assemble the notarytool credentials ---

notary_args=()
if [[ -n "${APPLE_API_KEY:-}" ]]; then
  api_key_file="${work_dir}/AuthKey.p8"
  printf '%s' "${APPLE_API_KEY}" | base64 --decode > "${api_key_file}"
  notary_args=(--key "${api_key_file}"
               --key-id "${APPLE_API_KEY_ID:?APPLE_API_KEY_ID is required}"
               --issuer "${APPLE_API_ISSUER_ID:?APPLE_API_ISSUER_ID is required}")
elif [[ -n "${APPLE_ID:-}" ]]; then
  notary_args=(--apple-id "${APPLE_ID}"
               --password "${APPLE_APP_PASSWORD:?APPLE_APP_PASSWORD is required}"
               --team-id "${APPLE_TEAM_ID:?APPLE_TEAM_ID is required}")
fi

if [[ "${identity}" != "-" && ${#notary_args[@]} -eq 0 ]]; then
  # Signing without notarizing is the worst of both worlds: it costs a release
  # slot and Gatekeeper still rejects the download. Treat it as a config error.
  echo "A signing certificate is configured but no notary credentials are" >&2
  echo "set. Provide APPLE_API_KEY/APPLE_API_KEY_ID/APPLE_API_ISSUER_ID or" >&2
  echo "APPLE_ID/APPLE_APP_PASSWORD/APPLE_TEAM_ID." >&2
  exit 1
fi

notarize() {
  local path="$1"
  local result="${work_dir}/notarize.json"
  local id status

  echo "Notarizing $(basename "${path}")..."
  if ! xcrun notarytool submit "${path}" "${notary_args[@]}" \
      --wait --timeout 30m --output-format json > "${result}"; then
    cat "${result}" >&2
    return 1
  fi

  # A rejected submission is still a successful *upload*, so the exit code
  # alone does not tell us whether Apple accepted the build.
  id="$(plutil -extract id raw "${result}")"
  status="$(plutil -extract status raw "${result}")"
  echo "Submission ${id}: ${status}"
  if [[ "${status}" != "Accepted" ]]; then
    # Only the log says which binary was rejected and why.
    xcrun notarytool log "${id}" "${notary_args[@]}" >&2 || true
    return 1
  fi
}

# --- Sign ---

if [[ "${identity}" == "-" ]]; then
  echo "warning: no signing certificate, producing an ad-hoc signed build" >&2
  codesign --sign - --force "${app}"
else
  # The hardened runtime and a secure timestamp are both hard requirements for
  # notarization. cargo-bundle produces a flat bundle with no nested code, so
  # signing the bundle covers the executable and --deep is unnecessary.
  codesign --sign "${identity}" --options runtime --timestamp --force "${app}"
  codesign --verify --strict --verbose=2 "${app}"
fi

# --- ZIP (portable archive) ---

# Notarization works on an archive, but the ticket is stapled to the bundle, so
# the zip has to be rebuilt afterwards to carry the stapled copy.
rm -f "${zip_path}"
ditto -c -k --sequesterRsrc --keepParent "${app}" "${zip_path}"

if [[ "${identity}" != "-" ]]; then
  notarize "${zip_path}"
  xcrun stapler staple "${app}"
  rm -f "${zip_path}"
  ditto -c -k --sequesterRsrc --keepParent "${app}" "${zip_path}"
fi

# --- DMG (drag-to-install) ---

staging="${work_dir}/dmg"
mkdir -p "${staging}"
# ditto rather than cp: it is the copy that keeps a signed bundle's metadata
# intact, including the notarization ticket stapled above.
ditto "${app}" "${staging}/FileMan.app"
ln -s /Applications "${staging}/Applications"
rm -f "${dmg_path}"
hdiutil create -volname FileMan -srcfolder "${staging}" -ov -format UDZO "${dmg_path}"

if [[ "${identity}" != "-" ]]; then
  # The disk image is quarantined on download and assessed in its own right, so
  # it needs a signature and a ticket of its own even though the app inside it
  # already has both.
  codesign --sign "${identity}" --timestamp --force "${dmg_path}"
  notarize "${dmg_path}"
  xcrun stapler staple "${dmg_path}"

  echo "Verifying Gatekeeper acceptance..."
  spctl --assess --type execute --verbose=2 "${app}"
  spctl --assess --type open --context context:primary-signature --verbose=2 "${dmg_path}"
fi

echo "Packaged:"
ls -la "${zip_path}" "${dmg_path}"
