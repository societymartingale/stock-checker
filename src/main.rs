use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use clap::Parser;
use num_format::{Locale, ToFormattedString};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use statrs::statistics::Statistics;
use tabled::{builder::Builder, settings::Style};
use textplots::{Chart, Plot, Shape};
use yfinance_rs::core::conversions::money_to_f64;
use yfinance_rs::fundamentals::CashflowRow;
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

    let (quotes_res, earnings_res, fi, cf) = tokio::join!(
        get_quotes(&ticker),
        get_earnings_dates(&ticker),
        ticker.fast_info(),
        ticker.cashflow(None)
    );
    let fi = fi?;
    let quotes = quotes_res?;
    let earnings = earnings_res.ok();
    let cf = cf?;

    if let Some(name) = fi.name {
        println!("{} ({})", name, &ags.ticker.to_uppercase());
    }

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

    println!("\n");
    display_plot(&quotes);
    print_cashflow(&cf);

    Ok(())
}

fn display_plot(quotes: &[Candle]) {
    if quotes.is_empty() {
        println!("No data to plot");
        return;
    }

    let prices: Vec<(f32, f32)> = quotes
        .iter()
        .enumerate()
        .map(|(i, c)| (i as f32, c.close.amount().to_f32().unwrap()))
        .collect();

    let xmax = (prices.len() - 1) as f32;
    let ymin = prices.iter().map(|(_, y)| *y).fold(f32::INFINITY, f32::min) * 0.99;
    let ymax = prices
        .iter()
        .map(|(_, y)| *y)
        .fold(f32::NEG_INFINITY, f32::max)
        * 1.01;
    Chart::new_with_y_range(180, 60, 0.0, xmax, ymin, ymax)
        .lineplot(&Shape::Steps(&prices))
        .nice();
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

fn print_cashflow(cf: &[CashflowRow]) {
    if cf.is_empty() {
        return;
    }
    println!();
    let mut builder = Builder::default();
    builder.push_record(["Year End", "Free Cash Flow"]);

    for item in cf {
        let period = &item.period.year_end();
        let fcf = &item.free_cash_flow;
        if let Some(period) = period {
            if let Some(fcf) = fcf {
                builder.push_record([
                    period.to_string(),
                    fcf.to_localized_string().unwrap().to_string(),
                ]);
            }
        }
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
