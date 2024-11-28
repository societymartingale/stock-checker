use statrs::statistics::Statistics;
use yahoo_finance_api::{self as yahoo, Quote};

const INTERVAL: &str = "1d";
const RANGE: &str = "14d";
const TICKER: &str = "MSTR";
const TRADING_DAYS_YEAR: f64 = 252.0; // assume 252 trading days per year

fn main() {
    let quotes = get_quotes(TICKER, INTERVAL, RANGE);
    let returns = calc_returns(&quotes);
    let mean_return = returns.as_slice().mean();
    let std_dev = returns
        .iter()
        .map(|r| r - mean_return)
        .collect::<Vec<f64>>()
        .as_slice()
        .std_dev();
    let annualized_vol = std_dev * TRADING_DAYS_YEAR.sqrt() * 100.0;

    println!("number of price quotes: {}", quotes.len());
    println!("std dev of returns: {}", std_dev);
    println!("annualized volatility: {}", annualized_vol);
}

fn get_quotes(ticker: &str, interval: &str, range: &str) -> Vec<Quote> {
    let yahoo = yahoo::YahooConnector::new().unwrap();
    tokio_test::block_on(yahoo.get_quote_range(ticker, interval, range))
        .unwrap()
        .quotes()
        .unwrap()
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
