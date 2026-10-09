use anyhow::Result;
use chrono::Utc;
use chrono::{DateTime, Datelike, NaiveDate};
use clap::{Parser, ValueEnum};
use num_format::{Locale, ToFormattedString};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use statrs::statistics::Statistics;
use tabled::{builder::Builder, settings::Style};
use textplots::{Chart, Plot, Shape};
use yfinance_rs::fundamentals::CashflowRow;
use yfinance_rs::{Candle, Interval, Range, Ticker, YfClientBuilder};
use yfinance_rs::{Currency, Money, PriceAmount, QuantityAmount, ReportingPeriod};

const CHART_HEIGHT: u32 = 60;
const CHART_WIDTH: u32 = 180;
const TRADING_DAYS_YEAR: f64 = 252.0; // assume 252 trading days per year
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36";

#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "lowercase")]
enum RangeArg {
    D1,
    D5,
    M1,
    M3,
    M6,
    Y1,
    Y2,
    Y5,
    Y10,
    Ytd,
    Max,
}

impl From<RangeArg> for Range {
    fn from(arg: RangeArg) -> Self {
        match arg {
            RangeArg::D1 => Range::D1,
            RangeArg::D5 => Range::D5,
            RangeArg::M1 => Range::M1,
            RangeArg::M3 => Range::M3,
            RangeArg::M6 => Range::M6,
            RangeArg::Y1 => Range::Y1,
            RangeArg::Y2 => Range::Y2,
            RangeArg::Y5 => Range::Y5,
            RangeArg::Y10 => Range::Y10,
            RangeArg::Ytd => Range::Ytd,
            RangeArg::Max => Range::Max,
        }
    }
}

#[derive(Parser, Debug)]
struct Args {
    #[arg(short, long, required = true, help = "ticker symbol such as MSFT")]
    ticker: String,
    #[arg(value_enum, short, long, default_value_t = RangeArg::M1, help = "historical time range")]
    range: RangeArg,
}

#[derive(Debug)]
struct PriceRange {
    low: f64,
    high: f64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let ags = Args::parse();
    let range: Range = ags.range.into();
    let client = YfClientBuilder::default().user_agent(USER_AGENT).build()?;
    let ticker = Ticker::new(&client, &ags.ticker);

    let (quotes, earnings, fi, cf, risk_free_rate) = tokio::join!(
        get_quotes(&ticker, range),
        get_earnings_dates(&ticker),
        ticker.fast_info(),
        ticker.cashflow(None),
        get_risk_free_rate(&client),
    );
    let fi = fi?;
    let quotes = quotes?;
    let earnings = earnings.ok();
    let cf = cf?;
    let risk_free_rate = risk_free_rate?;

    if let Some(name) = fi.snapshot.name {
        println!("{} ({})", name, ags.ticker.to_uppercase());
    }

    let returns = calc_returns(&quotes);
    print_quotes(&quotes, &returns);

    println!("\n");
    display_plot(&quotes);

    println!("\n--- Price Analysis ---");
    if quotes.len() >= 2 {
        let initial_close = close(&quotes[0]);
        if initial_close != Decimal::ZERO {
            let pct_chg = Decimal::from(100) * (close(&quotes[quotes.len() - 1]) - initial_close)
                / initial_close;
            println!("Pct change over period: {:.2}", pct_chg);
        }
    }

    if quotes.len() >= 3 {
        // need at least 3 data points to calculate std dev
        let std_dev = returns.as_slice().std_dev();
        let annualized_vol = std_dev * TRADING_DAYS_YEAR.sqrt() * 100.0;
        let sortino = sortino_ratio(&returns, risk_free_rate);
        println!("Std dev of returns: {:.4}", std_dev);
        println!("Annualized volatility: {:.2}", annualized_vol);
        println!(
            "Sortino ratio: {:.2} (using risk free rate of {:.2}%)",
            sortino,
            risk_free_rate * 100.0
        );
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
        if let Some(last) = fi.snapshot.last.as_ref() {
            let last = to_f64(rounded_price(last, &fi.snapshot.currency));
            if last < intraday.high {
                println!(
                    "Pct below intraday high for period: {:.2}",
                    100.0 * (intraday.high - last) / intraday.high
                )
            }
            if last < closing.high {
                println!(
                    "Pct below closing high for period: {:.2}",
                    100.0 * (closing.high - last) / closing.high
                )
            }
        }
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
    if quotes.is_empty() || quotes.len() < 2 {
        return;
    }

    let prices: Vec<(f32, f32)> = quotes
        .iter()
        .enumerate()
        .filter_map(|(i, c)| close(c).to_f32().map(|y| (i as f32, y)))
        .collect();

    if prices.len() < 2 {
        return;
    }
    let xmax = (quotes.len() - 1) as f32;
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
            format_volume(q.volume.as_ref()),
            format!("{:.2}", rounded_price(&q.ohlc.open, &q.currency)),
            format!("{:.2}", rounded_price(&q.ohlc.high, &q.currency)),
            format!("{:.2}", rounded_price(&q.ohlc.low, &q.currency)),
            format!("{:.2}", close(q)),
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
        let period = year_end(&item.period);
        if let (Some(period), Some(fcf)) = (period, item.free_cash_flow.as_ref()) {
            builder.push_record([period.to_string(), format_money(fcf)]);
        }
    }

    let table = builder.build().with(Style::sharp()).to_string();
    println!("{}", table);
}

/// Format a volume with thousands separators, e.g. "217,307,400".
/// Missing volume prints as an empty cell.
fn format_volume(volume: Option<&QuantityAmount>) -> String {
    let Some(volume) = volume else {
        return String::new();
    };
    let amount = volume.as_decimal();
    match amount.to_u64() {
        Some(whole) if amount.fract().is_zero() => whole.to_formatted_string(&Locale::en),
        _ => amount.normalize().to_string(),
    }
}

/// Format money with its currency symbol and separators, e.g. "$96,676,000,000.00".
/// Falls back to the plain "<amount> <code>" form if localized formatting fails.
fn format_money(money: &Money) -> String {
    money
        .to_localized_string()
        .unwrap_or_else(|_| money.to_string())
}

/// Fiscal year label used by the cash flow table: Dec 31 of the period's year.
/// Matches the `ReportingPeriod::year_end()` helper that paft 0.9 removed.
fn year_end(period: &ReportingPeriod) -> Option<NaiveDate> {
    let year = match period {
        ReportingPeriod::Date(date) => Some(date.get().year()),
        other => other.year(),
    }?;
    NaiveDate::from_ymd_opt(year, 12, 31)
}

/// Price rounded to the currency's minor units (cents for USD), as the old
/// `Money`-based candle fields were. Falls back to the raw amount if the
/// currency has no known precision.
fn rounded_price(price: &PriceAmount, currency: &Currency) -> Decimal {
    Money::new(*price.as_decimal(), currency.clone())
        .map(|m| m.amount())
        .unwrap_or(*price.as_decimal())
}

fn close(candle: &Candle) -> Decimal {
    rounded_price(&candle.ohlc.close, &candle.currency)
}

/// Lossy conversion for statistics; prices always fit in an f64.
fn to_f64(amount: Decimal) -> f64 {
    amount.to_f64().unwrap_or(f64::NAN)
}

async fn get_quotes(ticker: &Ticker, range: Range) -> Result<Vec<Candle>> {
    let hist = ticker
        .history(Some(range), Some(Interval::D1), false)
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
        let cur = to_f64(close(&quotes[i]));
        let prev = to_f64(close(&quotes[i - 1]));
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
        let low = to_f64(rounded_price(&q.ohlc.low, &q.currency));
        let high = to_f64(rounded_price(&q.ohlc.high, &q.currency));
        let close = to_f64(close(q));
        intraday.low = intraday.low.min(low);
        intraday.high = intraday.high.max(high);
        closing.low = closing.low.min(close);
        closing.high = closing.high.max(close);
    }

    Some((intraday, closing))
}

async fn get_risk_free_rate(client: &yfinance_rs::YfClient) -> Result<f64> {
    // 13 WEEK TREASURY BILL: ^IRX
    let ticker = Ticker::new(client, "^IRX");
    let fi = ticker.fast_info().await?;
    let last = fi
        .snapshot
        .last
        .ok_or_else(|| anyhow::anyhow!("Could not retrieve ^IRX price"))?;
    let rate = to_f64(rounded_price(&last, &fi.snapshot.currency)) / 100.0;
    Ok(rate)
}

fn sortino_ratio(returns: &[f64], risk_free_annual: f64) -> f64 {
    if returns.is_empty() {
        return 0.0;
    }

    let risk_free_daily = (1.0 + risk_free_annual).powf(1.0 / TRADING_DAYS_YEAR) - 1.0;
    let excess_returns: Vec<f64> = returns.iter().map(|r| r - risk_free_daily).collect();
    let mean_excess = excess_returns.as_slice().mean();
    let downside_variance = excess_returns
        .iter()
        .map(|r| if *r < 0.0 { r * r } else { 0.0 })
        .sum::<f64>()
        / excess_returns.len() as f64;

    let downside_std_dev = downside_variance.sqrt();
    if downside_std_dev < f64::EPSILON {
        return 0.0;
    }

    let annualization_factor = TRADING_DAYS_YEAR.sqrt();
    (mean_excess * TRADING_DAYS_YEAR) / (downside_std_dev * annualization_factor)
}

#[cfg(test)]
mod tests {
    use super::{
        calc_returns, format_money, format_volume, get_price_range, rounded_price, sortino_ratio,
        year_end,
    };
    use chrono::TimeZone;
    use paft_money::{Currency, IsoCurrency, Money, PriceAmount, QuantityAmount};
    use rust_decimal::Decimal;
    use yfinance_rs::{Candle, Ohlc};

    fn usd_currency() -> Currency {
        Currency::Iso(IsoCurrency::USD)
    }

    fn price(amount: &str) -> PriceAmount {
        PriceAmount::new(Decimal::from_str_exact(amount).unwrap())
    }

    fn candle(low: &str, high: &str, close: &str) -> Candle {
        let ts = chrono::Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap();
        let ohlc = Ohlc::new(price(close), price(high), price(low), price(close));
        let mut candle = Candle::new(ts, usd_currency(), ohlc);
        candle.volume = Some(QuantityAmount::from_decimal(Decimal::ONE).unwrap());
        candle
    }

    #[test]
    fn prices_round_to_cents_like_money_did() {
        assert_eq!(
            rounded_price(&price("236.05999755859375"), &usd_currency()).to_string(),
            "236.06"
        );
        assert_eq!(
            rounded_price(&price("230.475"), &usd_currency()).to_string(),
            "230.48"
        );
    }

    #[test]
    fn cash_flow_year_end_matches_old_labels() {
        use chrono::NaiveDate;
        use yfinance_rs::ReportingPeriod;

        let fy_end = NaiveDate::from_ymd_opt(2026, 1, 31).unwrap();
        let dec31 = NaiveDate::from_ymd_opt(2026, 12, 31);
        assert_eq!(year_end(&ReportingPeriod::date(fy_end).unwrap()), dec31);
        assert_eq!(year_end(&ReportingPeriod::annual(2026).unwrap()), dec31);
        assert_eq!(
            year_end(&ReportingPeriod::quarterly(2026, 3).unwrap()),
            dec31
        );
        assert_eq!(year_end(&ReportingPeriod::other("TTM").unwrap()), None);
    }

    #[test]
    fn volume_is_formatted_with_separators() {
        let vol = QuantityAmount::from_decimal(Decimal::from(217_307_400u64)).unwrap();
        assert_eq!(format_volume(Some(&vol)), "217,307,400");

        let fractional =
            QuantityAmount::from_decimal(Decimal::from_str_exact("12.50").unwrap()).unwrap();
        assert_eq!(format_volume(Some(&fractional)), "12.5");

        assert_eq!(format_volume(None), "");
    }

    #[test]
    fn free_cash_flow_is_formatted_as_localized_money() {
        let fcf = Money::new(Decimal::from(96_676_000_000u64), usd_currency()).unwrap();
        assert_eq!(format_money(&fcf), "$96,676,000,000.00");

        let negative = Money::new(Decimal::from(-1_234_500i64), usd_currency()).unwrap();
        assert_eq!(format_money(&negative), "-$1,234,500.00");
    }

    #[test]
    fn calc_returns_is_close_to_close_change() {
        let quotes = vec![
            candle("9", "11", "10"),
            candle("10", "13", "12"),
            candle("8", "12", "9"),
        ];

        let returns = calc_returns(&quotes);

        assert_eq!(returns.len(), 2);
        assert!((returns[0] - 0.2).abs() < 1e-12);
        assert!((returns[1] - (-0.25)).abs() < 1e-12);
    }

    #[test]
    fn price_range_tracks_intraday_and_close() {
        let quotes = vec![candle("9", "11", "10"), candle("8", "14", "12")];

        let (intraday, closing) = get_price_range(&quotes).expect("quotes are present");

        assert!((intraday.low - 8.0).abs() < 1e-12);
        assert!((intraday.high - 14.0).abs() < 1e-12);
        assert!((closing.low - 10.0).abs() < 1e-12);
        assert!((closing.high - 12.0).abs() < 1e-12);
    }

    #[test]
    fn empty_quotes_have_no_price_range() {
        assert!(get_price_range(&[]).is_none());
    }

    #[test]
    fn sortino_is_zero_without_downside() {
        assert_eq!(sortino_ratio(&[], 0.04), 0.0);
        assert_eq!(sortino_ratio(&[0.01, 0.02], 0.0), 0.0);
    }
}
