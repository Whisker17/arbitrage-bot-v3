# Ground-truth collector report (WHI-956)

- Schema: `whisker-arb/ground-truth-collector/v1`
- Block range: `96806569`–`98098684`
- Input: `arbs_month.jsonl`
- Candidates seen: 10501
- Accepted: 10501
- Distinct bots: 68
- Events fingerprint: `0x19f9b9df7dffb8d07b5c0cf16a0402228c0f121118d928c392487ed01830bcaa`
- Heuristic: `single_tx AND swap_events>=2 AND entity_net_positive_ge1 AND entity_net_negative_eq0 AND entity_gross_out AND msg_value_wei<=1e18 AND NOT liquidation AND NOT jit_lp AND NOT sandwich`

## Exclusion counts

| Category | Count |
| --- | ---: |
| cex_dex | 0 |
| liquidation | 0 |
| jit_lp | 0 |
| sandwich | 0 |
| insufficient_swaps | 0 |
| not_closed_cycle | 0 |
| no_gross_out | 0 |
| out_of_range | 0 |
| missing_fields | 0 |
| **total excluded** | 0 |

## Hop-count distribution

- `10`: 23
- `11`: 13
- `1139`: 1
- `12`: 21
- `13`: 14
- `14`: 10
- `15`: 1
- `16`: 2
- `17`: 4
- `185`: 1
- `2`: 3047
- `20`: 2
- `22`: 2
- `23`: 1
- `24`: 2
- `26`: 2
- `28`: 1
- `29`: 1
- `3`: 6768
- `31`: 1
- `32`: 1
- `34`: 1
- `4`: 155
- `40`: 1
- `43`: 1
- `44`: 2
- `5`: 122
- `52`: 1
- `57`: 2
- `58`: 1
- `6`: 121
- `67`: 1
- `7`: 98
- `8`: 47
- `85`: 1
- `9`: 29

## Funding distribution

- `self_funded`: 10501

## Settlement-asset distribution (top)

- `0x78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8`: 10264
- `0xcda86a272531e8640cd7f1a92c01839911b90bb0`: 86
- `0x201eba5cc46d216ce6dc03f6a759e8e766e956ae`: 73
- `0xe6829d9a7ee3040e1276fa75293bde931859e8fa`: 27
- `0x779ded0c9e1022225f8e0630b35a9b54be713736`: 12
- `0xdeaddeaddeaddeaddeaddeaddeaddeaddead1111`: 12
- `0x5d3a1ff2b6bab83b63cd9ad0787074081a52ef34`: 5
- `0x09bc4e0d864854c6afb6eb9a9cdf58ac190d0df9`: 4
- `0x111111d2bf19e43c34263401e0cad979ed1cdb61`: 4
- `0xc96de26018a54d51c097160568752c4e3bd6c364`: 4
- `native`: 4
- `0x00000000efe302beaa2b3e6e1b18d08d69a9012a`: 1
- `0x26a6b0dcdcfb981362afa56d581e4a7dba3be140`: 1
- `0x5be26527e817998a7206475496fde1e68957c5a6`: 1
- `0x787cb0d29194f0faca73884c383cf4d2501bb874`: 1

## Venue (factory) distribution (top)

- `0x25780dc8fc3cfbd75f33bfdab65e969b603b2035`: 6550
- `0x530d2766d1988cc1c000c8b7d00334c14b69ad71`: 4296
- `0x45e5f26451cdb01b0fa1f8582e0aad9a6f27c218`: 1997
- `0xeeca0a86431a7b42ca2ee5f479832c3d4a4c2644`: 1995
- `0xa6630671775c4ea2743840f9a5016dcf2a104054`: 1264
- `0x5bef015ca9424a7c07b68490616a4c1f094bedec`: 1089
- `0xf883162ed9c7e8ef604214c964c678e40c9b737c`: 1063
- `0xe5020961fa51ffd3662cdf307def18f9a87cce7c`: 756
- `0x636ea278699a300d3a849ab2ce36c891c4ee3da0`: 753
- `0x5c84e5d27fc7575d002fe98c5a1791ac3ce6fd2f`: 498
- `0x0d922fb1bc191f64970ac40376643808b4b74df9`: 372
- `0xc848bc597903b4200b9427a3d7f61e3ff0553913`: 253
- `0xaaa32926fce6be95ea2c51cb4fcb60836d320c42`: 176
- `0xd7d3e1116277a0f8b6f23cc64d5ea56982822dde`: 123
- `0x5b54d3610ec3f7fb1d5b42ccf4df0fb4e136f249`: 96

## Verification sample

- Method: rpc-receipt+swap-topic-count (Blockscout explorer.mantle.xyz returned 502 during run; same heuristic via eth_getTransactionReceipt)
- Sample size: 40
- True positive: 40
- False positive: 0
- Unverified: 0
- **Precision: 100.0%**
- Notes: Deterministic 40-tx stride sample over sorted accepted events. Each receipt: status=success, >=2 swap-family topics, msg.value<=1 MNT, no liquidation topic. Precision 40/40 = 100%.

## Notes

- Historical baseline: WHI-906 30-day arbs_month.jsonl (blocks 96,806,569–98,098,684).
- Input is pre-extracted atomic arbs (arb_extract.mjs); exclusion flags for liq/jit/sandwich/cex-dex were applied upstream — collector counts those only when present on the row.
- Venues resolved via external pool_census.json factories.
