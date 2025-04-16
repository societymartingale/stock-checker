use anyhow::Result;
use chrono::prelude::*;
use clap::Parser;
use num_format::{Locale, ToFormattedString};
use statrs::statistics::Statistics;
use tabled::{builder::Builder, settings::Style};
use yahoo_finance_api::{self as yahoo, Quote};

const INTERVAL: &str = "1d";
const TRADING_DAYS_YEAR: f64 = 252.0; // assume 252 trading days per year

#[derive(Parser, Debug)]
struct Args {
    #[arg(short, long, required = true, help = "ticker symbol such as MSFT")]
    ticker: String,
    #[arg(
        short,
        long,
        required = false,
        default_value_t = 10,
        help = "range in days"
    )]
    days: u8,
}

fn main() -> Result<()> {
    let ags = Args::parse();
    let range = format!("{}d", ags.days);
    let quotes = get_quotes(&ags.ticker, INTERVAL, &range)?;
    let returns = calc_returns(&quotes);
    print_quotes(&quotes, &returns);
    if quotes.len() >= 2 {
        let pct_chg = 100.0 * (quotes[quotes.len() - 1].close - quotes[0].close) / quotes[0].close;
        println!("pct change over period: {:.2}", pct_chg);
    }

    if quotes.len() >= 3 {
        // need at least 3 data points to calculate std dev
        let mean_return = returns.as_slice().mean();
        let std_dev = returns
            .iter()
            .map(|r| r - mean_return)
            .collect::<Vec<f64>>()
            .as_slice()
            .std_dev();
        let annualized_vol = std_dev * TRADING_DAYS_YEAR.sqrt() * 100.0;
        println!("std dev of returns: {:.4}", std_dev);
        println!("annualized volatility: {:.2}", annualized_vol);
    }

    Ok(())
}

fn print_quotes(quotes: &[Quote], returns: &[f64]) {
    let mut builder = Builder::default();
    builder.push_record(["Date", "Volume", "Open", "High", "Low", "Close", "Return %"]);
    for (idx, q) in quotes.iter().enumerate() {
        let mut ret_fmt = "".to_string();
        if idx > 0 {
            let ret = returns[idx - 1] * 100.0;
            if ret < 0.0 {
                ret_fmt = format!("{:.2}", ret);
            } else {
                ret_fmt = format!(" {:.2}", ret);
            }
        }

        builder.push_record([
            DateTime::from_timestamp(q.timestamp as i64, 0)
                .unwrap()
                .date_naive()
                .to_string(),
            q.volume.to_formatted_string(&Locale::en),
            format!("{:.2}", q.open),
            format!("{:.2}", q.high),
            format!("{:.2}", q.low),
            format!("{:.2}", q.close),
            ret_fmt,
        ]);
    }
    let table = builder.build().with(Style::sharp()).to_string();

    println!("{}", table);
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
