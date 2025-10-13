# 通用操作指南

## 合约部署

### WMNT 的 deposit/withdraw

以下示例均依赖 `cast`，请确保已正确配置：

- `export RPC_URL="https://rpc.mantle-sepolia.testnet"`（替换为目标网络 RPC）
- `export PK="<你的私钥>"`
- Mantle Sepolia WMNT 地址：`0x67A1f4A939b477A6b7c5BF94D97E45dE87E608eF`
- Mantle Mainnet WMNT 地址：`0x78C1b0C915C4fAA5fFfA6cABf0219DA63d7F4Cb8`

**包装 MNT 为 WMNT**（示例：20 WMNT）：

```
WMNT_ADDRESS=0x67A1f4A939b477A6b7c5BF94D97E45dE87E608eF
cast send "$WMNT_ADDRESS" "deposit()" --value $(cast --to-wei 20 ether) --rpc-url "$RPC_URL" --private-key "$PK"
```

**赎回 WMNT**（示例：赎回 5 WMNT）：

```
WMNT_ADDRESS=0x67A1f4A939b477A6b7c5BF94D97E45dE87E608eF
cast send "$WMNT_ADDRESS" "withdraw(uint256)" $(cast --to-wei 5 ether) --rpc-url "$RPC_URL" --private-key "$PK"
```

### 合约注资

将已包装的 WMNT 注入 `ArbitrageExecutor` 合约（示例：20 WMNT）：

```
WMNT_ADDRESS=0x67A1f4A939b477A6b7c5BF94D97E45dE87E608eF
EXECUTOR_ADDRESS=<你的执行器地址>
FUND_AMOUNT=$(cast --to-wei 20 ether)
cast send "$WMNT_ADDRESS" "transfer(address,uint256)(bool)" "$EXECUTOR_ADDRESS" "$FUND_AMOUNT" --rpc-url "$RPC_URL" --private-key "$PK"
```

### 合约资金提取

`ArbitrageExecutor` 提供了两种提取方式：

1. **提取指定数量**：调用 `withdrawAmount(address,uint256)`

   ```
   EXECUTOR_ADDRESS=<你的执行器地址>
   WMNT_ADDRESS=0x67A1f4A939b477A6b7c5BF94D97E45dE87E608eF
   AMOUNT=$(cast --to-wei 2 ether) # 示例：2 WMNT
   cast send "$EXECUTOR_ADDRESS" "withdrawAmount(address,uint256)" "$WMNT_ADDRESS" "$AMOUNT" --rpc-url "$RPC_URL" --private-key "$PK"
   ```

2. **提取全部余额**：调用 `withdraw(address)`

   ```
   EXECUTOR_ADDRESS=<你的执行器地址>
   WMNT_ADDRESS=0x67A1f4A939b477A6b7c5BF94D97E45dE87E608eF
   cast send "$EXECUTOR_ADDRESS" "withdraw(address)" "$WMNT_ADDRESS" --rpc-url "$RPC_URL" --private-key "$PK"
   ```

### 合约 WMNT 余额查询

查询任意地址的 WMNT 余额：

```
WMNT_ADDRESS=0x67A1f4A939b477A6b7c5BF94D97E45dE87E608eF
TARGET=<要查询的地址，例如 $EXECUTOR_ADDRESS>
cast call "$WMNT_ADDRESS" "balanceOf(address)(uint256)" "$TARGET" --rpc-url "$RPC_URL"
```

## 合约功能性测试

