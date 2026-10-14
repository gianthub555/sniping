use {log::Level, std::io::Write};



pub fn redact_urls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("http") {
        let (before, candidate) = rest.split_at(start);
        out.push_str(before);
        if !(candidate.starts_with("http://") || candidate.starts_with("https://")) {
            out.push_str("http");
            rest = &candidate[4..];
            continue;
        }
        let end = candidate
            .find(|c: char| c.is_whitespace() || matches!(c, ')' | '"' | '\'' | ',' | '>'))
            .unwrap_or(candidate.len());
        let url = &candidate[..end];
        match url.split_once('?') {
            Some((base, query)) if !query.is_empty() => {
                out.push_str(base);
                out.push_str("?***");
            }
            _ => out.push_str(url),
        }
        rest = &candidate[end..];
    }
    out.push_str(rest);
    out
}


pub fn init_logger() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format(|buf, record| {
            let style = buf.default_level_style(record.level());
            let level = match record.level() {
                Level::Error => "ERROR",
                Level::Warn => "WARN ",
                Level::Info => "INFO ",
                Level::Debug => "DEBUG",
                Level::Trace => "TRACE",
            };
            writeln!(
                buf,
                "[{} {style}{level}{style:#} {}] {}",
                buf.timestamp(),
                record.target(),
                redact_urls(&record.args().to_string())
            )
        })
        .init();
}

#[cfg(test)]
mod tests {
    use super::redact_urls;

    #[test]
    fn hides_query_strings() {
        assert_eq!(
            redact_urls("error for url (https://mainnet.helius-rpc.com/?api-key=abc123)"),
            "error for url (https://mainnet.helius-rpc.com/?***)"
        );
        assert_eq!(redact_urls("http://ewr1.nozomi.temporal.xyz/?c=KEY failed"), "http://ewr1.nozomi.temporal.xyz/?*** failed");
        assert_eq!(redact_urls("no url, just httpx text"), "no url, just httpx text");
        assert_eq!(redact_urls("https://a.b/c and https://d.e/?k=1"), "https://a.b/c and https://d.e/?***");
    }
}
