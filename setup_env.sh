#!/bin/bash

# Setup script for arbitrage execution environment
# This script helps you create the .env file safely

set -e

echo "================================================"
echo "🔧 Arbitrage Bot Environment Setup"
echo "================================================"
echo ""

# Check if .env already exists
if [ -f .env ]; then
    echo "⚠️  Warning: .env file already exists!"
    read -p "Do you want to overwrite it? (yes/no): " confirm
    if [ "$confirm" != "yes" ]; then
        echo "Setup cancelled."
        exit 0
    fi
    echo ""
fi

# Get private key
echo "📝 Please enter your wallet private key"
echo "   (WITHOUT the 0x prefix)"
echo "   ⚠️  Make sure this is a test wallet with limited funds!"
echo ""
read -sp "Private Key: " PRIVATE_KEY
echo ""
echo ""

# Validate private key length (should be 64 hex characters)
if [ ${#PRIVATE_KEY} -ne 64 ]; then
    echo "❌ Error: Private key should be 64 characters (32 bytes in hex)"
    echo "   Your input length: ${#PRIVATE_KEY}"
    exit 1
fi

# Get RPC URL
#
# WHI-745 / WHI-526: public free-tier Mantle endpoints are NOT suitable for the
# multi-protocol shadow/gate window. Documented failure modes include multi-
# address eth_getLogs 429s, type-0x7e receipt decode gaps, missing blocks when
# HTTP/WS are mixed across providers, and WS silent stalls.
#
# Qualify any candidate with:
#   cargo run --release --bin rpc_probe -- \
#     --http "$MANTLE_RPC_URL" --ws "$MANTLE_RPC_WS_URL" \
#     --blocks 128 --duration 600 --out evidence/rpc/<label>.json
# Reports (labels + fingerprints only) live under evidence/rpc/ (see STATUS.md).
#
# Preferred path for gate / live dry-run: option 4 (custom production pair) with
# matched HTTP+WS from one provider, then a green rpc_probe report before WHI-535.
echo "🌐 Choose RPC endpoint:"
echo "   1) https://rpc.mantle.xyz"
echo "      ⚠ public — NOT gate-qualified (WHI-526/WHI-745); dev/smoke only"
echo "   2) https://mantle.publicnode.com"
echo "      ⚠ public — NOT gate-qualified (429 / 0x7e issues recorded)"
echo "   3) https://rpc.ankr.com/mantle"
echo "      ⚠ public — NOT gate-qualified; free-tier not sized for merged universe"
echo "   4) Custom production URL (recommended for gate / live dry-run)"
echo "      Supply matched HTTP + WS from one provider; run rpc_probe before WHI-535"
echo ""
read -p "Select option (1-4) [default: 4]: " rpc_choice
rpc_choice=${rpc_choice:-4}

# Always reset WS so a leftover env var cannot silently mix providers.
RPC_WS_URL=""

confirm_public_dev_only() {
    echo "⚠️  This public endpoint is NOT gate-qualified (WHI-526 / WHI-745)."
    echo "   Casual/dev smoke only. Do not start WHI-535 on this pair."
    echo "   Gate / live dry-run requires option 4 + a green rpc_probe report."
    read -p "Type 'dev-only' to continue, or anything else to abort: " public_confirm
    if [ "$public_confirm" != "dev-only" ]; then
        echo "Setup cancelled (public endpoint not confirmed as dev-only)."
        exit 0
    fi
}

case $rpc_choice in
    1)
        confirm_public_dev_only
        RPC_URL="https://rpc.mantle.xyz"
        # Intentionally no matched WS — public menu items are HTTP-only.
        ;;
    2)
        confirm_public_dev_only
        RPC_URL="https://mantle.publicnode.com"
        ;;
    3)
        confirm_public_dev_only
        RPC_URL="https://rpc.ankr.com/mantle"
        ;;
    4)
        read -p "Enter custom HTTP RPC URL: " RPC_URL
        read -p "Enter matching WS RPC URL (same provider): " RPC_WS_URL
        echo "Next: qualify with rpc_probe before any WHI-535 gate window"
        echo "  (see evidence/rpc/STATUS.md)."
        ;;
    *)
        echo "Invalid option — defaulting to custom (no public endpoint assumed)"
        read -p "Enter custom HTTP RPC URL: " RPC_URL
        read -p "Enter matching WS RPC URL (same provider): " RPC_WS_URL
        ;;
esac

echo ""
echo "Selected HTTP RPC: $RPC_URL"
if [ -n "$RPC_WS_URL" ]; then
    echo "Selected WS RPC:   $RPC_WS_URL"
else
    echo "Selected WS RPC:   (unset — set MANTLE_RPC_WS_URL to a matched same-provider URL)"
fi
echo ""

# Create .env file
echo "💾 Creating .env file..."
cat > .env << EOF
# Arbitrage Bot Configuration
# Generated on $(date)

# SECURITY WARNING: Never commit this file to version control!

# Private key for transaction signing (without 0x prefix)
PRIVATE_KEY=$PRIVATE_KEY

# RPC endpoint for Mantle network (HTTP)
RPC_URL=$RPC_URL

# Optional: Mantle-specific RPC (alternative names)
MANTLE_RPC_URL=$RPC_URL
MANTLE_PROVIDER_URL=$RPC_URL

# Matched WebSocket endpoint (same provider as HTTP). Required for state_space
# subscriptions and for rpc_probe Check C/E. Do NOT mix providers.
MANTLE_RPC_WS_URL=$RPC_WS_URL

# Optional: numbered candidates for WHI-745-style qualification (labels are the
# only values safe to quote in issues/PRs; never commit real URLs).
# MANTLE_RPC_CANDIDATE_1_LABEL=my-provider
# MANTLE_RPC_CANDIDATE_1_HTTP=
# MANTLE_RPC_CANDIDATE_1_WS=
EOF

# Set proper permissions (owner read/write only)
chmod 600 .env

echo "✅ .env file created successfully!"
echo ""
echo "📋 Configuration Summary:"
echo "   Private Key: [HIDDEN FOR SECURITY]"
echo "   RPC URL: $RPC_URL"
echo "   File permissions: -rw------- (600)"
echo ""

# Verify .env is in .gitignore
if [ -f .gitignore ]; then
    if grep -q "^\.env$" .gitignore; then
        echo "✅ .env is already in .gitignore"
    else
        echo "⚠️  Adding .env to .gitignore..."
        echo ".env" >> .gitignore
        echo "✅ .env added to .gitignore"
    fi
else
    echo "⚠️  No .gitignore found, creating one..."
    echo ".env" > .gitignore
    echo "✅ .gitignore created"
fi

echo ""
echo "================================================"
echo "🚀 Setup Complete!"
echo "================================================"
echo ""
echo "Next steps:"
echo "1. Verify your wallet has sufficient balance:"
echo "   - At least 0.52 WMNT for the arbitrage input"
echo "   - Sufficient MNT for gas fees"
echo ""
echo "2. Verify the pinned gas profile without submitting a transaction:"
echo "   cargo run --locked --example verify_gas_profile_runtime"
echo ""
echo "⚠️  IMPORTANT REMINDERS:"
echo "   - Never share your private key"
echo "   - Never commit .env to git"
echo "   - Only use test funds initially"
echo "   - Monitor gas prices before execution"
echo ""
