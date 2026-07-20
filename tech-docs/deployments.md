# Executor Deployment History

This ledger preserves the deployment and funding facts recorded by the Foundry
broadcast artifacts removed in WHI-508. Addresses and transaction hashes are
kept verbatim; `run-latest.json` files were duplicate pointers to the preceding
timestamped run and are not listed separately.

The source commit values are short IDs recorded in the original broadcast
metadata. Some are historical commits that are not reachable from this checkout.

## Deployments

| Date (UTC) | Chain | Contract address | Deployment transaction | Block | Recorded source commit |
| --- | ---: | --- | --- | --- | --- |
| 2025-10-06 10:05:51 | Mantle Sepolia (5003) | `0x0a4385950ec05102862dca3eaf5f2bc717bd73b0` | `0x8afc23114d20862ca416a94c1f72663445b531afeba75ee13ad48c3bf5fa3b07` | `0x1bc08cf` | `a0363d1` |
| 2025-10-07 09:12:21 | Mantle Sepolia (5003) | `0x64fc6c07cf1ec56e0875b08b9020775894eb7ec7` | `0xab83779170b9e2d2e628b4f8b42b4efdbe9a1131512bedbd9ad1f7b748d87e5e` | `0x1bcab4a` | `a0363d1` |
| 2025-10-07 09:33:23 | Mantle Sepolia (5003) | `0x26f34ad8edf6fcb4e44d1e21109fc33ff42406ea` | `0xdd2a1bcdd5abf6edff9e600f7d51a4f889ae08c2d873d0091bace5140cb30c96` | `0x1bcadc1` | `a0363d1` |
| 2025-10-11 16:41:12 | Mantle Sepolia (5003) | `0x59e5019b0d0e40762df46fe472c0ae5a5c80b80f` | `0x0d9b88db295f56bf5ea54e195e751c5d543676d57be064453b8bb97cd656f200` | `0x1bf82e3` | `1698746` |
| 2025-10-13 07:15:28 | Mantle Sepolia (5003) | `0xe3fe72b3286ba305571de96631120a4046ebf97c` | `0x3999a8f0e49190264f4da5e49bc6762f8db537aa2915f34d8115db63bc474da9` | `0x1c09217` | `5ad8478` |
| 2025-10-13 08:07:09 | Mantle Mainnet (5000) | `0xe3fe72b3286ba305571de96631120a4046ebf97c` | `0xf3dd67261ea5ffb827b05240910a84cabf2c692b408061128db7152fb32aa7bc` | `0x521e092` | `5ad8478` |
| 2025-10-21 16:43:30 | Mantle Mainnet (5000) | `0x5714c6bca610c16594c8ab3f812e42ebbb062077` | `0xf9edc98c2a5a22ab519c2082a9c73fbe12ef45b1e0975224c895975a7e7afd25` | `0x5276315` | `4c660b7` |

All recorded deployment receipts succeeded (`status = 0x1`). The same address
can appear on both chains because deployments are chain-local.

## Funding transactions

The funding scripts deposited native value into the configured wrapped-native
token and transferred wrapped-native tokens to the executor. Amounts below are
the raw integer values recorded in the broadcast arguments.

| Date (UTC) | Chain | Executor funded | Native deposit transaction | Token transfer transaction | Amount |
| --- | ---: | --- | --- | --- | ---: |
| 2025-10-06 10:07:09 | Mantle Sepolia (5003) | `0x0a4385950ec05102862dca3eaf5f2bc717bd73b0` | `0xb2eb2b7f4a097820b869d4333c5889f7b440cbc52ca5702a4be73d1e84befdbf` | `0xc2f1a213c50d1e67b89fb4d17adda3cb644929e636976e8ece652d1e5ecf0d50` | `100000000000000000` |
| 2025-10-06 10:09:42 | Mantle Sepolia (5003) | `0x0a4385950ec05102862dca3eaf5f2bc717bd73b0` | `0xb4a8a8de4b6225eda99b1732e7b6c04d31af54ac58559b6c82ccf2a4c5b58b20` | `0x871640f3e75f17e43c8ae312d37e15381ea196d37ca6c8191e0d920a85d11268` | `100000000000000000` |
| 2025-10-06 10:11:41 | Mantle Sepolia (5003) | `0x0a4385950ec05102862dca3eaf5f2bc717bd73b0` | `0x82b0f7324d719e619797b20b810ccd65f6a4b999c6ff55d8fdaafbeffe6375f4` | `0xb427d863f9eebcd918c7731403786e636d40cde621409c99ec3639988bd17d1a` | `20000000000000000000` |
| 2025-10-13 07:16:08 | Mantle Sepolia (5003) | `0xe3fe72b3286ba305571de96631120a4046ebf97c` | `0x30af32abd37c10893ef484ec5c08761a2cc53e23b2a9884f9539d4fc9b2593f6` | `0x081bf13f22f806218b225a460c48facc5db1301ba33aa30b3686727a6b905e20` | `20000000000000000000` |
| 2025-10-13 08:08:07 | Mantle Mainnet (5000) | `0xe3fe72b3286ba305571de96631120a4046ebf97c` | `0x6800e58d4ac7033ffb8aa8db5654917d49c6c4db2ffa20881adb0b796e416a85` | `0xeb8b67ac9bd0ca7ecc8eefceb7a3b283d097562fee8d38305db226b84a1a0156` | `1000000000000000000` |

The later Mantle Mainnet deployment at `0x5714...` has no matching funding
run in the preserved broadcast history; the 2025-10-13 Mainnet funding run
targeted the earlier `0xe3fe...` deployment.
