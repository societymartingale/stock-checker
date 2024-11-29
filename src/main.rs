use anyhow::Result;
use chrono::prelude::*;
use polars::prelude::*;
use statrs::statistics::Statistics;
use yahoo_finance_api::{self as yahoo, Decimal, Quote};

const INTERVAL: &str = "1d";
const RANGE: &str = "1mo";
const TICKER: &str = "MSTR";
const TRADING_DAYS_YEAR: f64 = 252.0; // assume 252 trading days per year

fn main() -> Result<()> {
    let quotes = get_quotes(TICKER, INTERVAL, RANGE)?;
    let returns = calc_returns(&quotes);
    let mean_return = returns.as_slice().mean();
    let std_dev = returns
        .iter()
        .map(|r| r - mean_return)
        .collect::<Vec<f64>>()
        .as_slice()
        .std_dev();
    let annualized_vol = std_dev * TRADING_DAYS_YEAR.sqrt() * 100.0;

    print_quotes(&quotes);
    println!("std dev of returns: {}", std_dev);
    println!("annualized volatility: {}", annualized_vol);

    Ok(())
}

fn print_quotes(quotes: &[Quote]) {
    let dates: Vec<NaiveDate> = quotes
        .iter()
        .map(|q| {
            DateTime::from_timestamp(q.timestamp as i64, 0)
                .unwrap()
                .date_naive()
        })
        .collect();

    let df: DataFrame = df!(
        "timestamp" => dates,
        "open" => quotes.iter().map(|q| q.open).collect::<Vec<Decimal>>(),
        "high" => quotes.iter().map(|q| q.high).collect::<Vec<Decimal>>(),
        "low" => quotes.iter().map(|q| q.low).collect::<Vec<Decimal>>(),
        "volume" => quotes.iter().map(|q| q.volume).collect::<Vec<u64>>(),
        "close" => quotes.iter().map(|q| q.close).collect::<Vec<Decimal>>(),

    )
    .unwrap();

    println!("{}", df);
}

fn get_quotes(ticker: &str, interval: &str, range: &str) -> Result<Vec<Quote>> {
    let yahoo = yahoo::YahooConnector::new()?;
    let res = tokio_test::block_on(yahoo.get_quote_range(ticker, interval, range))?;
    Ok(res.quotes()?)
}

fn calc_returns(quotes: &[Quote]) -> Vec<f64> {
    let mut res: Vec<f64> = vec![];
    for i in 1..quotes.len() {
        let cur = quotes[i].close;
        let prev = quotes[i - 1].close;
        res.push((cur - prev) / prev);
    }
    res
}
