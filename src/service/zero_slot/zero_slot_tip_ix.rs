use {
    crate::service::utils::tip::tip_instruction,
    solana_instruction::Instruction,
    solana_pubkey::Pubkey,
};


pub const ZERO_SLOT_MIN_TIP_LAMPORTS: u64 = 1_000_000;
pub const ZERO_SLOT_ADVANCED_MIN_TIP_LAMPORTS: u64 = 100_000;

pub const ZERO_SLOT_TIP_ACCOUNTS: [Pubkey; 21] = [
    Pubkey::from_str_const("6fQaVhYZA4w3MBSXjJ81Vf6W1EDYeUPXpgVQ6UQyU1Av"),
    Pubkey::from_str_const("4HiwLEP2Bzqj3hM2ENxJuzhcPCdsafwiet3oGkMkuQY4"),
    Pubkey::from_str_const("7toBU3inhmrARGngC7z6SjyP85HgGMmCTEwGNRAcYnEK"),
    Pubkey::from_str_const("8mR3wB1nh4D6J9RUCugxUpc6ya8w38LPxZ3ZjcBhgzws"),
    Pubkey::from_str_const("6SiVU5WEwqfFapRuYCndomztEwDjvS5xgtEof3PLEGm9"),
    Pubkey::from_str_const("TpdxgNJBWZRL8UXF5mrEsyWxDWx9HQexA9P1eTWQ42p"),
    Pubkey::from_str_const("D8f3WkQu6dCF33cZxuAsrKHrGsqGP2yvAHf8mX6RXnwf"),
    Pubkey::from_str_const("GQPFicsy3P3NXxB5piJohoxACqTvWE9fKpLgdsMduoHE"),
    Pubkey::from_str_const("Ey2JEr8hDkgN8qKJGrLf2yFjRhW7rab99HVxwi5rcvJE"),
    Pubkey::from_str_const("4iUgjMT8q2hNZnLuhpqZ1QtiV8deFPy2ajvvjEpKKgsS"),
    Pubkey::from_str_const("3Rz8uD83QsU8wKvZbgWAPvCNDU6Fy8TSZTMcPm3RB6zt"),
    Pubkey::from_str_const("DiTmWENJsHQdawVUUKnUXkconcpW4Jv52TnMWhkncF6t"),
    Pubkey::from_str_const("HRyRhQ86t3H4aAtgvHVpUJmw64BDrb61gRiKcdKUXs5c"),
    Pubkey::from_str_const("7y4whZmw388w1ggjToDLSBLv47drw5SUXcLk6jtmwixd"),
    Pubkey::from_str_const("J9BMEWFbCBEjtQ1fG5Lo9kouX1HfrKQxeUxetwXrifBw"),
    Pubkey::from_str_const("8U1JPQh3mVQ4F5jwRdFTBzvNRQaYFQppHQYoH38DJGSQ"),
    Pubkey::from_str_const("Eb2KpSC8uMt9GmzyAEm5Eb1AAAgTjRaXWFjKyFXHZxF3"),
    Pubkey::from_str_const("FCjUJZ1qozm1e8romw216qyfQMaaWKxWsuySnumVCCNe"),
    Pubkey::from_str_const("ENxTEjSQ1YabmUpXAdCgevnHQ9MHdLv8tzFiuiYJqa13"),
    Pubkey::from_str_const("6rYLG55Q9RpsPGvqdPNJs4z5WTxJVatMB8zV3WJhs5EK"),
    Pubkey::from_str_const("Cix2bHfqPcKcM233mzxbLk14kSggUUiz2A87fJtGivXr"),
];

pub fn zero_slot_tip_instruction(payer: &Pubkey, lamports: u64) -> Instruction {
    tip_instruction(payer, &ZERO_SLOT_TIP_ACCOUNTS, lamports)
}
