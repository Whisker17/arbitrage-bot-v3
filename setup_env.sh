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
echo "🌐 Choose RPC endpoint:"
echo "   1) https://rpc.mantle.xyz (default)"
echo "   2) https://mantle.publicnode.com"
echo "   3) https://rpc.ankr.com/mantle"
echo "   4) Custom URL"
echo ""
read -p "Select option (1-4) [default: 1]: " rpc_choice
rpc_choice=${rpc_choice:-1}

case $rpc_choice in
    1)
        RPC_URL="https://rpc.mantle.xyz"
        ;;
    2)
        RPC_URL="https://mantle.publicnode.com"
        ;;
    3)
        RPC_URL="https://rpc.ankr.com/mantle"
        ;;
    4)
        read -p "Enter custom RPC URL: " RPC_URL
        ;;
    *)
        echo "Invalid option, using default"
        RPC_URL="https://rpc.mantle.xyz"
        ;;
esac

echo ""
echo "Selected RPC: $RPC_URL"
echo ""

# Create .env file
echo "💾 Creating .env file..."
cat > .env << EOF
# Arbitrage Bot Configuration
# Generated on $(date)

# SECURITY WARNING: Never commit this file to version control!

# Private key for transaction signing (without 0x prefix)
PRIVATE_KEY=$PRIVATE_KEY

# RPC endpoint for Mantle network
RPC_URL=$RPC_URL

# Optional: Mantle-specific RPC (alternative names)
MANTLE_RPC_URL=$RPC_URL
MANTLE_PROVIDER_URL=$RPC_URL
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
