#!/usr/bin/env bash
# Sourced after _common.sh. Keep release and development service selection together.

configure_ios_variant() {
  MAPLE_IOS_VARIANT="${MAPLE_IOS_VARIANT:-production}"
  case "${MAPLE_IOS_VARIANT}" in
    production)
      use_release_environment
      ;;
    dev)
      use_pr_environment
      VITE_MAPLE_DEV_AUTH_ORIGIN="$(python3 "${SCRIPT_DIR}/ios-build-profile.py" validate-auth-origin "${MAPLE_IOS_DEV_AUTH_ORIGIN:-}")"
      export VITE_MAPLE_DEV_AUTH_ORIGIN
      ;;
    *)
      echo "MAPLE_IOS_VARIANT must be production or dev." >&2
      return 1
      ;;
  esac
  # The environment helpers intentionally remove inherited VITE_* variables.
  export MAPLE_IOS_VARIANT
  export VITE_MAPLE_APP_VARIANT="${MAPLE_IOS_VARIANT}"
}
