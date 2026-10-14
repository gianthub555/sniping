use {
    crate::service::utils::tip::tip_instruction,
    solana_instruction::Instruction,
    solana_pubkey::Pubkey,
};


pub const NOZOMI_MIN_TIP_LAMPORTS: u64 = 1_000_000;

pub const NOZOMI_TIP_ACCOUNTS: [Pubkey; 17] = [
    Pubkey::from_str_const("TEMPaMeCRFAS9EKF53Jd6KpHxgL47uWLcpFArU1Fanq"),
    Pubkey::from_str_const("noz3jAjPiHuBPqiSPkkugaJDkJscPuRhYnSpbi8UvC4"),
    Pubkey::from_str_const("noz3str9KXfpKknefHji8L1mPgimezaiUyCHYMDv1GE"),
    Pubkey::from_str_const("noz6uoYCDijhu1V7cutCpwxNiSovEwLdRHPwmgCGDNo"),
    Pubkey::from_str_const("noz9EPNcT7WH6Sou3sr3GGjHQYVkN3DNirpbvDkv9YJ"),
    Pubkey::from_str_const("nozc5yT15LazbLTFVZzoNZCwjh3yUtW86LoUyqsBu4L"),
    Pubkey::from_str_const("nozFrhfnNGoyqwVuwPAW4aaGqempx4PU6g6D9CJMv7Z"),
    Pubkey::from_str_const("nozievPk7HyK1Rqy1MPJwVQ7qQg2QoJGyP71oeDwbsu"),
    Pubkey::from_str_const("noznbgwYnBLDHu8wcQVCEw6kDrXkPdKkydGJGNXGvL7"),
    Pubkey::from_str_const("nozNVWs5N8mgzuD3qigrCG2UoKxZttxzZ85pvAQVrbP"),
    Pubkey::from_str_const("nozpEGbwx4BcGp6pvEdAh1JoC2CQGZdU6HbNP1v2p6P"),
    Pubkey::from_str_const("nozrhjhkCr3zXT3BiT4WCodYCUFeQvcdUkM7MqhKqge"),
    Pubkey::from_str_const("nozrwQtWhEdrA6W8dkbt9gnUaMs52PdAv5byipnadq3"),
    Pubkey::from_str_const("nozUacTVWub3cL4mJmGCYjKZTnE9RbdY5AP46iQgbPJ"),
    Pubkey::from_str_const("nozWCyTPppJjRuw2fpzDhhWbW355fzosWSzrrMYB1Qk"),
    Pubkey::from_str_const("nozWNju6dY353eMkMqURqwQEoM3SFgEKC6psLCSfUne"),
    Pubkey::from_str_const("nozxNBgWohjR75vdspfxR5H9ceC7XXH99xpxhVGt3Bb"),
];

pub fn nozomi_tip_instruction(payer: &Pubkey, lamports: u64) -> Instruction {
    tip_instruction(payer, &NOZOMI_TIP_ACCOUNTS, lamports)
}
