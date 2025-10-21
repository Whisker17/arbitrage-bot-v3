# MOE 集成指南

## Prerequisite

1. 适配 MOE LBT 的 payload
    - 参考 /Users/whisker/Work/src/Whisker17/arbitrage/arbitrage-bot-v3/contracts/src/Agni 这里的实现，在 /Users/whisker/Work/src/Whisker17/arbitrage/arbitrage-bot-v3/contracts/src/Moe 完成 NOE LBT 的相关 payload，而 MOE LBT 的合约设计可以在这里看到 /Users/whisker/Work/src/Whisker17/arbitrage/arbitrage-bot-v3/data/contracts/moe
    - 当然还有 interface 的设计，你需要在这里也补充好 /Users/whisker/Work/src/Whisker17/arbitrage/arbitrage-bot-v3/contracts/src/interfaces
    - 在这里 /Users/whisker/Work/src/Whisker17/arbitrage/arbitrage-bot-v3/examples/moe 写好测试脚本，确保 payload 的可用性

## 功能实现

1. 监控服务构建，我需要你构建一个基于 MOE LBT 的套利路径监控和计算的服务，你可以参考 /Users/whisker/Work/src/Whisker17/arbitrage/arbitrage-bot-v3/examples/test/monitor_pools_for_v3.rs 这里的实现（但是这个文件实现的是对 Agni 协议的，你需要适配成 MOE LBT），请你将实现好的服务放在 /Users/whisker/Work/src/Whisker17/arbitrage/arbitrage-bot-v3/examples/moe
