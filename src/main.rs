use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use clap::Parser;
use num_format::{Locale, ToFormattedString};
use rust_decimal::Decimal;
use statrs::statistics::Statistics;
use tabled::{builder::Builder, settings::Style};
use yfinance_rs::core::conversions::money_to_f64;
use yfinance_rs::{Candle, Interval, Range, Ticker, YfClientBuilder};

const TRADING_DAYS_YEAR: f64 = 252.0; // assume 252 trading days per year
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36";

#[derive(Parser, Debug)]
struct Args {
    #[arg(short, long, required = true, help = "ticker symbol such as MSFT")]
    ticker: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let ags = Args::parse();
    let client = YfClientBuilder::default().user_agent(USER_AGENT).build()?;
    let ticker = Ticker::new(&client, &ags.ticker);
    let (quotes_res, earnings_res) = tokio::join!(get_quotes(&ticker), get_earnings_dates(&ticker));
    let quotes = quotes_res?;
    let earnings = earnings_res.ok();

    let returns = calc_returns(&quotes);
    print_quotes(&quotes, &returns);
    if quotes.len() >= 2 {
        let pct_chg = Decimal::from(100)
            * (quotes[quotes.len() - 1].close.amount() - quotes[0].close.amount())
            / quotes[0].close.amount();
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

    if let Some(er) = earnings {
        if !er.is_empty() {
            println!("earnings date: {}", er[0].format("%Y-%m-%d %H:%M"));
        }
    }

    Ok(())
}

fn print_quotes(quotes: &[Candle], returns: &[f64]) {
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
            q.ts.date_naive().to_string(),
            q.volume.unwrap().to_formatted_string(&Locale::en),
            format!("{:.2}", q.open.amount()),
            format!("{:.2}", q.high.amount()),
            format!("{:.2}", q.low.amount()),
            format!("{:.2}", q.close.amount()),
            ret_fmt,
        ]);
    }
    let table = builder.build().with(Style::sharp()).to_string();

    println!("{}", table);
}

async fn get_quotes(ticker: &Ticker) -> Result<Vec<Candle>> {
    let hist = ticker
        .history(Some(Range::M1), Some(Interval::D1), false)
        .await?;
    Ok(hist)
}

async fn get_earnings_dates(ticker: &Ticker) -> Result<Vec<DateTime<Utc>>> {
    let cal = ticker.calendar().await?;
    let earnings = cal.earnings_dates;
    Ok(earnings)
}

fn calc_returns(quotes: &[Candle]) -> Vec<f64> {
    let mut res: Vec<f64> = vec![];
    for i in 1..quotes.len() {
        let cur = money_to_f64(&quotes[i].close);
        let prev = money_to_f64(&quotes[i - 1].close);
        res.push((cur - prev) / prev);
    }
    res
}
