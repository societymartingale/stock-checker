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

const CHART_HEIGHT: u32 = 60;
const CHART_WIDTH: u32 = 180;
const TRADING_DAYS_YEAR: f64 = 252.0; // assume 252 trading days per year
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36";

#[derive(Parser, Debug)]
struct Args {
    #[arg(short, long, required = true, help = "ticker symbol such as MSFT")]
    ticker: String,
}

#[derive(Debug)]
struct PriceRange {
    low: f64,
    high: f64,
}

#[tokio::main]
async fn main() -> Result<()> {
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

    println!("\n");
    display_plot(&quotes);

    println!("\n--- Price Analysis ---");
    if quotes.len() >= 2 {
        let initial_close = quotes[0].close.amount();
        if initial_close != Decimal::ZERO {
            let pct_chg = Decimal::from(100)
                * (quotes[quotes.len() - 1].close.amount() - initial_close)
                / initial_close;
            println!("Pct change over period: {:.2}", pct_chg);
        }
    }

    if quotes.len() >= 3 {
        // need at least 3 data points to calculate std dev
        let std_dev = returns.as_slice().std_dev();
        let annualized_vol = std_dev * TRADING_DAYS_YEAR.sqrt() * 100.0;
        println!("Std dev of returns: {:.4}", std_dev);
        println!("Annualized volatility: {:.2}", annualized_vol);
    }

    if let Some((intraday, closing)) = get_price_range(&quotes) {
        println!(
            "Intraday low and high: {:.2} to {:.2}",
            intraday.low, intraday.high
        );
        println!(
            "Closing low and high:  {:.2} to {:.2}",
            closing.low, closing.high
        );
    }

    if let Some(er) = earnings {
        if !er.is_empty() {
            println!("Earnings date: {}", er[0].format("%Y-%m-%d %H:%M"));
        }
    }

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
    Chart::new_with_y_range(CHART_WIDTH, CHART_HEIGHT, 0.0, xmax, ymin, ymax)
        .lineplot(&Shape::Steps(&prices))
        .nice();
}

fn print_quotes(quotes: &[Candle], returns: &[f64]) {
    if quotes.is_empty() {
        println!("No quotes to display");
        return;
    }

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

fn get_price_range(quotes: &[Candle]) -> Option<(PriceRange, PriceRange)> {
    // get intraday and closing price ranges over time period
    if quotes.is_empty() {
        return None;
    }

    let mut intraday = PriceRange {
        low: f64::INFINITY,
        high: f64::NEG_INFINITY,
    };
    let mut closing = PriceRange {
        low: f64::INFINITY,
        high: f64::NEG_INFINITY,
    };
    for q in quotes {
        let low = money_to_f64(&q.low);
        let high = money_to_f64(&q.high);
        let close = money_to_f64(&q.close);
        intraday.low = intraday.low.min(low);
        intraday.high = intraday.high.max(high);
        closing.low = closing.low.min(close);
        closing.high = closing.high.max(close);
    }

    Some((intraday, closing))
}
