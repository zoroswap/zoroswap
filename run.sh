#!/usr/bin/env bash
set -euo pipefail

export MIDEN_NODE_ENDPOINT=localhost
export RUST_LOG=debug,tower=warn,h2=warn,miden_client=warn

# Updates faucet_id in the [[liquidity_pools]] block matching the given symbol.
# Usage: update_faucet_id <symbol> <new_faucet_id>
update_faucet_id() {
    local symbol="$1"
    local new_id="$2"
    # Scan config.toml for a line matching symbol = "<sym>", then replace the
    # quoted value on the next faucet_id line with the new id.
    awk -v sym="$symbol" -v nid="$new_id" '
        /^symbol *= *"/ { if ($0 ~ "\"" sym "\"") found=1 }
        found && /^faucet_id *= *"/ { sub(/"[^"]*"$/, "\"" nid "\""); found=0 }
        { print }
    ' config.toml > config.toml.tmp && mv -f config.toml.tmp config.toml
}

echo "=== Step 1: Spawn faucets ==="
FAUCET_OUTPUT=$(cargo run --release --bin spawn_faucets 2>&1) || { echo "$FAUCET_OUTPUT"; exit 1; }
echo "$FAUCET_OUTPUT"

# Parse faucet IDs from output lines like:
#   Faucet account ID (BTC): "mlcl1az..."
for symbol in BTC ETH USDC; do
    faucet_id=$(echo "$FAUCET_OUTPUT" | grep "Faucet account ID ($symbol):" | sed 's/.*"\(.*\)".*/\1/')
    if [ -n "$faucet_id" ]; then
        echo "Setting $symbol faucet_id = $faucet_id"
        update_faucet_id "$symbol" "$faucet_id"
    else
        echo "WARNING: Could not parse faucet ID for $symbol"
    fi
done

echo ""
echo "=== Step 2: Spawn pools ==="
POOL_OUTPUT=$(cargo run --release --bin spawn_pools 2>&1) || { echo "$POOL_OUTPUT"; exit 1; }
echo "$POOL_OUTPUT"

# Parse pool ID from output line like:
#   New pool created: "mlcl1az..."
pool_id=$(echo "$POOL_OUTPUT" | grep "New pool created:" | sed 's/.*"\(.*\)".*/\1/')
if [ -n "$pool_id" ]; then
    echo "Setting pool_account_id = $pool_id"
    sed -i '' "s/^pool_account_id = .*/pool_account_id = \"$pool_id\"/" config.toml
else
    echo "WARNING: Could not parse pool ID"
fi

echo ""
echo "=== Step 3: Run server ==="
cargo run --release --bin server --features zoro-curve-local
