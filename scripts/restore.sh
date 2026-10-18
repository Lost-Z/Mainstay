#!/usr/bin/env bash
# =============================================================================
# Mainstay Restore Script
# =============================================================================
# Restores assets, engineers, and maintenance records from a backup produced
# by scripts/backup.sh.  Supports both local and S3 backup sources.
#
# Usage:
#   ./scripts/restore.sh <backup_path_or_s3_uri> [--dry-run] [--verify-only]
#
#   backup_path_or_s3_uri   - Path to local backup dir or s3:// URI
#   --dry-run               - Show what would be restored without executing
#   --verify-only           - Only validate backup integrity, do not restore
#   --admin-key             - Path to the admin Stellar key (required for writes)
#   --owner-key-dir <dir>   - Directory of per-owner Stellar secret key files
#                             named <owner>.key (owner-authorized restore)
#   --restore-source <mode> - "owner" (default) or "admin" (admin-gated restore)
#   --start-asset-id <N>    - Only restore assets with ID >= N
#
# Environment variables:
#   CONTRACT_ASSET_REGISTRY     - Asset Registry contract ID
#   CONTRACT_ENGINEER_REGISTRY  - Engineer Registry contract ID
#   CONTRACT_LIFECYCLE          - Lifecycle contract ID
#   STELLAR_NETWORK             - Stellar network (testnet/mainnet)
#   STELLAR_RPC_URL             - Stellar RPC endpoint
#   AWS_ACCESS_KEY_ID           - AWS access key (for S3 sources)
#   AWS_SECRET_ACCESS_KEY       - AWS secret key (for S3 sources)
#   AWS_DEFAULT_REGION          - AWS region
# =============================================================================

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

source ~/.cargo/env 2>/dev/null || true
source .env 2>/dev/null || true

: "${STELLAR_NETWORK:=testnet}"
: "${AWS_DEFAULT_REGION:=us-east-1}"

DRY_RUN=false
VERIFY_ONLY=false
ADMIN_KEY_PATH=""
OWNER_KEY_DIR=""
RESTORE_SOURCE="owner"
START_ASSET_ID=1
ID_MAP_FILE=""

usage() {
    cat <<EOF
Usage: $0 <backup_path_or_s3_uri> [options]

Options:
  --dry-run              Preview what would be restored
  --verify-only          Validate backup integrity only (no writes)
  --admin-key <path>     Path to the admin Stellar secret key file
  --owner-key-dir <dir>  Directory of per-owner secret key files (<owner>.key)
  --restore-source <m>   "owner" (default) or "admin" (admin-gated restore)
  --start-asset-id <N>   Only restore assets with ID >= N (default: 1)

EOF
    exit 1
}

# Parse arguments
if [[ $# -lt 1 ]]; then
    usage
fi

BACKUP_SOURCE="$1"
shift

while [[ $# -gt 0 ]]; do
    case "$1" in
        --dry-run) DRY_RUN=true; shift ;;
        --verify-only) VERIFY_ONLY=true; shift ;;
        --admin-key) ADMIN_KEY_PATH="$2"; shift 2 ;;
        --owner-key-dir) OWNER_KEY_DIR="$2"; shift 2 ;;
        --restore-source) RESTORE_SOURCE="$2"; shift 2 ;;
        --start-asset-id) START_ASSET_ID="$2"; shift 2 ;;
        *) echo "Unknown option: $1"; usage ;;
    esac
done

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

log() { echo "[$(date -u +%H:%M:%S)] $*"; }
err() { echo "[$(date -u +%H:%M:%S)] ERROR: $*" >&2; }

resolve_backup_source() {
    local src="$1"
    local work_dir

    if [[ "$src" == s3://* ]]; then
        log "Downloading backup from S3: $src"
        work_dir="$(mktemp -d)"
        aws s3 cp "$src" "$work_dir" --recursive
        echo "$work_dir"
    else
        if [[ ! -d "$src" ]]; then
            err "Backup directory not found: $src"
            exit 1
        fi
        echo "$src"
    fi
}

verify_integrity() {
    local dir="$1"
    log "Verifying backup integrity..."

    # Check for checksums file
    if [[ -f "$dir/checksums.sha256" ]]; then
        log "Verifying SHA-256 checksums..."
        pushd "$dir" > /dev/null
        if sha256sum -c checksums.sha256 --quiet 2>/dev/null; then
            log "  All checksums verified OK"
        else
            err "  Checksum verification FAILED"
            return 1
        fi
        popd > /dev/null
    else
        log "  No checksums file found; skipping hash verification"
    fi

    # Check for required files
    local required=("manifest.json" "asset_count.json")
    local missing=()
    for f in "${required[@]}"; do
        if [[ ! -f "$dir/$f" ]]; then
            missing+=("$f")
        fi
    done
    if [[ ${#missing[@]} -gt 0 ]]; then
        err "Missing required files: ${missing[*]}"
        return 1
    fi

    # Verify manifest
    if ! jq -e . "$dir/manifest.json" > /dev/null 2>&1; then
        err "manifest.json is not valid JSON"
        return 1
    fi

    log "Backup integrity check passed."
    return 0
}

# Import a Stellar secret key under a given alias and echo the alias.
import_key() {
    local alias="$1"
    local key_file="$2"
    if [[ ! -f "$key_file" ]]; then
        err "Key file not found for '$alias': $key_file"
        return 1
    fi
    stellar keys import "$alias" --secret-key "$(cat "$key_file")" --network "$STELLAR_NETWORK" >/dev/null 2>&1 || true
    echo "$alias"
}

# Resolve the signing source for a given asset owner.
# Echoes the --source flag value, or returns non-zero if no key is available.
resolve_source_for_owner() {
    local owner="$1"
    if [[ "$RESTORE_SOURCE" == "admin" ]]; then
        if [[ -z "$ADMIN_KEY_PATH" ]]; then
            err "Admin-gated restore requires --admin-key"
            return 1
        fi
        import_key admin "$ADMIN_KEY_PATH" >/dev/null || return 1
        echo "admin"
        return 0
    fi
    # Owner-authorized path: look up the owner's key file.
    if [[ -z "$OWNER_KEY_DIR" ]]; then
        err "Owner-authorized restore requires --owner-key-dir"
        return 1
    fi
    local key_file="$OWNER_KEY_DIR/${owner}.key"
    if [[ ! -f "$key_file" ]]; then
        err "No key file for owner '$owner' (expected $key_file)"
        return 1
    fi
    import_key "owner_${owner}" "$key_file"
}

# invoke_write <contract_id> <func> <source> [args...]
# On success, echoes the raw contract output (stdout) so callers can parse
# return values.  On failure, echoes the captured error text and returns 1.
invoke_write() {
    local contract_id="$1"
    local func="$2"
    local source="$3"
    shift 3
    if [[ "$DRY_RUN" == "true" ]]; then
        log "[DRY RUN] stellar contract invoke --id $contract_id --network $STELLAR_NETWORK --source $source -- $func $*"
        return 0
    fi
    local output
    if output="$(stellar contract invoke \
        --id "$contract_id" \
        --network "$STELLAR_NETWORK" \
        --source "$source" \
        -- "$func" "$@" 2>&1)"; then
        echo "$output"
        return 0
    fi
    echo "$output"
    return 1
}

# ---------------------------------------------------------------------------
# Restore Functions
# ---------------------------------------------------------------------------

restore_assets() {
    local dir="$1"
    local assets_dir="$dir/assets"

    if [[ ! -d "$assets_dir" ]]; then
        log "No assets directory found; skipping asset restore."
        return 0
    fi

    log "Restoring assets..."

    local restored=0
    local would_restore=0
    local skipped=0
    local failed=0

    ID_MAP_FILE="$dir/id_map.json"
    if [[ "$DRY_RUN" != "true" ]]; then
        echo '{}' > "$ID_MAP_FILE"
    fi

    for asset_file in "$assets_dir"/*.json; do
        [[ -f "$asset_file" ]] || continue

        local asset_id
        asset_id="$(basename "$asset_file" .json)"

        if [[ "$asset_id" -lt "$START_ASSET_ID" ]]; then
            skipped=$((skipped + 1))
            continue
        fi

        # Extract asset fields from JSON
        local asset_type metadata serial_number owner
        asset_type="$(jq -r '.asset_type // empty' "$asset_file")"
        metadata="$(jq -r '.metadata // empty' "$asset_file")"
        serial_number="$(jq -r '.serial_number // empty' "$asset_file")"
        owner="$(jq -r '.owner // empty' "$asset_file")"

        if [[ -z "$asset_type" || -z "$metadata" || -z "$serial_number" || -z "$owner" ]]; then
            log "  WARNING: Asset $asset_id has incomplete data; skipping"
            failed=$((failed + 1))
            continue
        fi

        if [[ "$DRY_RUN" == "true" ]]; then
            local source
            source="$(resolve_source_for_owner "$owner")" || {
                log "  WARNING: Cannot determine signing source for asset $asset_id (owner $owner)"
                failed=$((failed + 1))
                continue
            }
            invoke_write "$CONTRACT_ASSET_REGISTRY" register_asset "$source" \
                --asset_type "$asset_type" \
                --metadata "$metadata" \
                --serial_number "$serial_number" \
                --owner "$owner" >/dev/null
            would_restore=$((would_restore + 1))
            continue
        fi

        local source
        source="$(resolve_source_for_owner "$owner")" || {
            log "  WARNING: Cannot determine signing source for asset $asset_id (owner $owner)"
            failed=$((failed + 1))
            continue
        }

        local output new_id
        if output="$(invoke_write "$CONTRACT_ASSET_REGISTRY" register_asset "$source" \
            --asset_type "$asset_type" \
            --metadata "$metadata" \
            --serial_number "$serial_number" \
            --owner "$owner")"; then
            new_id="$(printf '%s' "$output" | tr -d '[:space:]"')"
            if [[ -z "$new_id" || ! "$new_id" =~ ^[0-9]+$ ]]; then
                log "  WARNING: Could not parse new asset ID for asset $asset_id (output: $output)"
                failed=$((failed + 1))
                continue
            fi
            local tmp_map
            tmp_map="$(mktemp)"
            jq --arg old "$asset_id" --arg new "$new_id" '.[$old] = ($new | tonumber)' \
                "$ID_MAP_FILE" > "$tmp_map" && mv "$tmp_map" "$ID_MAP_FILE"
            restored=$((restored + 1))
        else
            log "  WARNING: Failed to restore asset $asset_id: $output"
            failed=$((failed + 1))
        fi
    done

    if [[ "$DRY_RUN" == "true" ]]; then
        log "Asset restore complete (dry run): $would_restore would restore, $skipped skipped, $failed failed"
        return 0
    fi

    # Validate that every restored asset has an ID mapping.
    local missing_map=0
    for asset_file in "$assets_dir"/*.json; do
        [[ -f "$asset_file" ]] || continue
        local asset_id
        asset_id="$(basename "$asset_file" .json)"
        if [[ "$asset_id" -lt "$START_ASSET_ID" ]]; then
            continue
        fi
        if ! jq -e --arg old "$asset_id" 'has($old)' "$ID_MAP_FILE" >/dev/null 2>&1; then
            log "  ERROR: No ID mapping for asset $asset_id"
            missing_map=$((missing_map + 1))
        fi
    done
    if [[ "$missing_map" -gt 0 ]]; then
        err "ID map incomplete: $missing_map asset(s) missing from $ID_MAP_FILE"
        return 1
    fi

    log "Asset restore complete: $restored restored, $skipped skipped, $failed failed"
    log "ID map written to $ID_MAP_FILE"
}

restore_maintenance() {
    local dir="$1"
    local maint_dir="$dir/maintenance"

    if [[ ! -d "$maint_dir" ]]; then
        log "No maintenance directory found; skipping."
        return 0
    fi

    log "Restoring maintenance records..."

    if [[ "$DRY_RUN" == "true" ]]; then
        log "[DRY RUN] Would re-link maintenance history using ID map (not yet written)"
        return 0
    fi

    if [[ -z "$ID_MAP_FILE" || ! -f "$ID_MAP_FILE" ]]; then
        err "ID map not found; cannot re-link maintenance history"
        return 1
    fi

    local plan_file="$dir/relink_plan.json"
    echo '[]' > "$plan_file"
    local unmapped=0

    for hist_file in "$maint_dir"/*_history.json; do
        [[ -f "$hist_file" ]] || continue
        local old_id
        old_id="$(basename "$hist_file" _history.json)"
        local new_id
        new_id="$(jq -r --arg old "$old_id" '.[$old] // empty' "$ID_MAP_FILE")"
        if [[ -z "$new_id" ]]; then
            log "  ERROR: No ID mapping for maintenance history of asset $old_id"
            unmapped=$((unmapped + 1))
            continue
        fi
        local tmp_plan
        tmp_plan="$(mktemp)"
        jq --arg old "$old_id" --arg new "$new_id" --arg file "$hist_file" \
            '. + [{old_id: ($old | tonumber), new_id: ($new | tonumber), history_file: $file}]' \
            "$plan_file" > "$tmp_plan" && mv "$tmp_plan" "$plan_file"
    done

    if [[ "$unmapped" -gt 0 ]]; then
        err "Cannot re-link maintenance history: $unmapped file(s) have no ID mapping"
        return 1
    fi

    log "Maintenance re-link plan written to $plan_file"
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

main() {
    log "=== Mainstay Restore ==="
    log "Network: ${STELLAR_NETWORK}"
    log "Backup source: ${BACKUP_SOURCE}"
    log "Dry run: ${DRY_RUN}"
    log "Verify only: ${VERIFY_ONLY}"

    # Resolve backup source (download from S3 if needed)
    local work_dir
    work_dir="$(resolve_backup_source "$BACKUP_SOURCE")"

    # Always verify integrity first
    if ! verify_integrity "$work_dir"; then
        err "Backup integrity check failed. Aborting."
        exit 1
    fi

    if [[ "$VERIFY_ONLY" == "true" ]]; then
        log "Verification complete. No state was modified."
        exit 0
    fi

    if [[ "$DRY_RUN" != "true" ]]; then
        if [[ "$RESTORE_SOURCE" == "admin" ]]; then
            if [[ -z "$ADMIN_KEY_PATH" ]]; then
                err "--admin-key is required for admin-gated restoration (or use --dry-run to preview)."
                exit 1
            fi
        elif [[ -z "$OWNER_KEY_DIR" ]]; then
            err "--owner-key-dir is required for owner-authorized restoration (or use --dry-run to preview)."
            exit 1
        fi
    fi

    log "Starting restoration..."

    # 1. Restore assets from backup
    restore_assets "$work_dir"

    # 2. Re-link maintenance records against the new IDs
    restore_maintenance "$work_dir"

    log "=== Restore complete ==="
}

main "$@"
