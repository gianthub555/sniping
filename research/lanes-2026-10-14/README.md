# Lane delivery study, 14 Oct 2026 (Frankfurt + Germany logs/rival_buys.csv)

10,943 outside buys in the create's block (13-14 Oct, both bots), by the landing service the buyer tipped:

| service | buys | ms after the create p10 / p25 / p50 / p90 | share of first outside buyers |
|---|---|---|---|
| no tip (direct / staked) | 5759 | 8 / 20 / 38 / 128 | 56% |
| Helius Sender | 2989 | 14 / 26 / 47 / 127 | 26% |
| Jito | 1076 | 7 / 22 / 45 / 105 | 9% |
| Nozomi | 859 | 10 / 22 / 44 / 141 | 8% |
| 0slot | 251 | 25 / 38 / 60 / 134 | 1% |
| Everstake | ~0 | - | 0% |

Nozomi fastest on Harmonic (p50 53 vs 65-85) and JitoAgave (29 vs 41-58); no-tip buys land earlier with a higher fee
(DE+NL p50 53 ms at <1 lamport/CU, 31 at 10-50, 22 at 200+). First outside buyer: p50 22-26 ms after the create (DE/NL).
Our 27 live buys (one lane each, rotation 0slot/everstake, no nonce fan-out): a third within 1-15 ms of the first rival,
the rest 30-200 ms behind; 0slot typical gap ~60 positions, Everstake ~120.
Proposed: fan-out on (nonce accounts), lanes Nozomi + Helius Sender + Jito (+ 0slot, Everstake), order nozomi,helius,jito.
