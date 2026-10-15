use anyhow::Result;
use chrono::Utc;
use chrono::{DateTime, Datelike, NaiveDate};
use chrono_tz::America::New_York;
use clap::{Parser, ValueEnum};
use num_format::{Locale, ToFormattedString};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use statrs::statistics::Statistics;
use tabled::{builder::Builder, settings::Style};
use textplots::{Chart, Plot, Shape};
use yfinance_rs::fundamentals::CashflowRow;
use yfinance_rs::{Candle, FastInfo, Interval, Range, Ticker, YfClientBuilder};
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
    #[arg(
        long,
        value_name = "RATE",
        help = "annual risk-free rate override, for example 0.04 (skips ^IRX)"
    )]
    risk_free: Option<f64>,
}

#[derive(Debug)]
struct PriceRange {
    low: f64,
    high: f64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let range: Range = args.range.into();
    let client = YfClientBuilder::default().user_agent(USER_AGENT).build()?;
    let ticker = Ticker::new(&client, &args.ticker);
    let risk_free_override = args.risk_free;

    let (quotes, earnings, quote_info, cash_flow, fetched_risk_free) = tokio::join!(
        get_quotes(&ticker, range),
        get_earnings_dates(&ticker),
        ticker.fast_info(),
        ticker.cashflow(None),
        async {
            if risk_free_override.is_some() {
                None
            } else {
                Some(get_risk_free_rate(&client).await)
            }
        },
    );
    // Price history is the body of the report. The other requests are optional
    // and must not discard a history that already succeeded.
    let quotes = quotes?;
    let earnings = earnings.ok();
    let quote_info = quote_info.ok();
    let risk_free_rate =
        resolve_risk_free_rate(risk_free_override, fetched_risk_free.and_then(Result::ok));

    print_heading(quote_info.as_ref(), &args.ticker);

    let returns = calc_returns(&quotes);
    print_quotes(&quotes, &returns);

    println!("\n");
    display_plot(&quotes);

    let last = quote_info.as_ref().and_then(|info| {
        let price = info.snapshot.last.as_ref()?;
        Some((price, &info.snapshot.currency))
    });
    print_price_analysis(&quotes, &returns, last, risk_free_rate);

    if let Some(first) = earnings.as_ref().and_then(|er| er.first()) {
        println!("Earnings date: {}", format_eastern(first));
    }

    print_cashflow_section(cash_flow);
    Ok(())
}

fn print_heading(quote_info: Option<&FastInfo>, ticker: &str) {
    let symbol = ticker.to_uppercase();
    if let Some(info) = quote_info {
        if let Some(name) = info.snapshot.name.as_deref() {
            println!("{name} ({symbol})");
        }
    } else {
        println!("Quote info unavailable ({symbol})");
    }
}

fn print_price_analysis(
    quotes: &[Candle],
    returns: &[f64],
    last: Option<(&PriceAmount, &Currency)>,
    risk_free_rate: Option<f64>,
) {
    println!("\n--- Price Analysis ---");
    if quotes.len() >= 2 {
        let initial_close = close(&quotes[0]);
        if initial_close != Decimal::ZERO {
            let pct_chg = Decimal::from(100) * (close(&quotes[quotes.len() - 1]) - initial_close)
                / initial_close;
            println!("Pct change over period: {pct_chg:.2}");
        }
    }

    let finite_returns = finite_returns(returns);
    if finite_returns.len() >= 2 {
        // need at least 3 data points to calculate std dev
        let std_dev = finite_returns.as_slice().std_dev();
        let annualized_vol = std_dev * TRADING_DAYS_YEAR.sqrt() * 100.0;
        println!("Std dev of returns: {std_dev:.4}");
        println!("Annualized volatility: {annualized_vol:.2}");
        println!("{}", format_sortino(&finite_returns, risk_free_rate));
    } else if risk_free_rate.is_none() {
        println!("Risk-free rate unavailable");
    }

    if let Some((intraday, closing)) = get_price_range(quotes) {
        println!(
            "Intraday low and high: {:.2} to {:.2}",
            intraday.low, intraday.high
        );
        println!(
            "Closing low and high:  {:.2} to {:.2}",
            closing.low, closing.high
        );
        if let Some((price, currency)) = last {
            let last = to_f64(rounded_price(price, currency));
            print_pct_below("intraday", last, intraday.high);
            print_pct_below("closing", last, closing.high);
        }
    }
}

fn print_pct_below(which: &str, last: f64, high: f64) {
    if last.is_finite() && high.is_finite() && high > 0.0 && last < high {
        println!(
            "Pct below {which} high for period: {:.2}",
            100.0 * (high - last) / high
        );
    }
}

fn print_cashflow_section<E>(cash_flow: Result<Vec<CashflowRow>, E>) {
    match cash_flow {
        Ok(rows) => print_cashflow(&rows),
        Err(_) => println!("\nCash flow unavailable"),
    }
}

/// Prefer `--risk-free` over the fetched ^IRX rate. `None` means Sortino has no rate.
fn resolve_risk_free_rate(override_rate: Option<f64>, fetched: Option<f64>) -> Option<f64> {
    match override_rate {
        Some(rate) => rate.is_finite().then_some(rate),
        None => fetched.filter(|rate| rate.is_finite()),
    }
}

fn format_sortino(returns: &[f64], risk_free_rate: Option<f64>) -> String {
    let Some(rate) = risk_free_rate else {
        return "Risk-free rate unavailable\nSortino ratio: n/a".to_string();
    };
    let pct = rate * 100.0;
    match sortino_ratio(returns, rate) {
        Some(sortino) => {
            format!("Sortino ratio: {sortino:.2} (using risk free rate of {pct:.2}%)")
        }
        None => format!("Sortino ratio: n/a (using risk free rate of {pct:.2}%)"),
    }
}

/// Format a UTC timestamp in US Eastern time (EST/EDT as appropriate), e.g. "2026-05-20 16:00 ET".
fn format_eastern(ts: &DateTime<Utc>) -> String {
    format!(
        "{} ET",
        ts.with_timezone(&New_York).format("%Y-%m-%d %H:%M")
    )
}

fn display_plot(quotes: &[Candle]) {
    if quotes.is_empty() || quotes.len() < 2 {
        return;
    }

    let prices: Vec<(f32, f32)> = quotes
        .iter()
        .filter_map(|c| decimal_to_f32(close(c)))
        .enumerate()
        .map(|(i, y)| (i as f32, y))
        .collect();

    if prices.len() < 2 {
        return;
    }
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
        let ret_fmt = if idx == 0 {
            String::new()
        } else {
            returns
                .get(idx - 1)
                .copied()
                .map_or_else(String::new, format_return_cell)
        };

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

fn format_money(money: &Money) -> String {
    localized_or_canonical(money.to_localized_string(), || money.to_string())
}

fn localized_or_canonical<E>(
    localized: Result<String, E>,
    canonical: impl FnOnce() -> String,
) -> String {
    match localized {
        Ok(formatted) => formatted,
        Err(_) => canonical(),
    }
}

fn format_return_cell(ret: f64) -> String {
    if !ret.is_finite() {
        return String::new();
    }
    let pct = ret * 100.0;
    if pct < 0.0 {
        format!("{pct:.2}")
    } else {
        format!(" {pct:.2}")
    }
}

fn finite_returns(returns: &[f64]) -> Vec<f64> {
    returns
        .iter()
        .copied()
        .filter(|ret| ret.is_finite())
        .collect()
}

/// Plot coordinate. `None` when the price does not fit in a finite `f32`.
fn decimal_to_f32(amount: Decimal) -> Option<f32> {
    amount.to_f32().filter(|value| value.is_finite())
}

/// Cash-flow table label is Dec 31 of the fiscal year, not the period date.
/// A fiscal year ending 2026-01-31 still prints as 2026-12-31.
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
    let mut res = Vec::with_capacity(quotes.len().saturating_sub(1));
    for i in 1..quotes.len() {
        let cur = to_f64(close(&quotes[i]));
        let prev = to_f64(close(&quotes[i - 1]));
        // Keep one slot per pair so the table stays aligned. A zero previous
        // close would divide by zero; a missing conversion is not a return.
        if prev.is_finite() && prev != 0.0 && cur.is_finite() {
            res.push((cur - prev) / prev);
        } else {
            res.push(f64::NAN);
        }
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

    if !intraday.low.is_finite()
        || !intraday.high.is_finite()
        || !closing.low.is_finite()
        || !closing.high.is_finite()
    {
        return None;
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
    if !rate.is_finite() {
        return Err(anyhow::anyhow!("Could not retrieve ^IRX price"));
    }
    Ok(rate)
}

fn sortino_ratio(returns: &[f64], risk_free_annual: f64) -> Option<f64> {
    if !risk_free_annual.is_finite() {
        return None;
    }

    let risk_free_daily = (1.0 + risk_free_annual).powf(1.0 / TRADING_DAYS_YEAR) - 1.0;
    if !risk_free_daily.is_finite() {
        return None;
    }
    let excess_returns: Vec<f64> = returns
        .iter()
        .copied()
        .filter(|ret| ret.is_finite())
        .map(|ret| ret - risk_free_daily)
        .collect();
    if excess_returns.is_empty() {
        return None;
    }

    let mean_excess = excess_returns.as_slice().mean();
    let downside_variance = excess_returns
        .iter()
        .map(|r| if *r < 0.0 { r * r } else { 0.0 })
        .sum::<f64>()
        / excess_returns.len() as f64;

    let downside_std_dev = downside_variance.sqrt();
    if !downside_std_dev.is_finite() || downside_std_dev < f64::EPSILON {
        return None;
    }

    let annualization_factor = TRADING_DAYS_YEAR.sqrt();
    Some((mean_excess * TRADING_DAYS_YEAR) / (downside_std_dev * annualization_factor))
}

#[cfg(test)]
mod tests {
    use super::{
        calc_returns, format_eastern, format_money, format_return_cell, format_sortino,
        format_volume, get_price_range, localized_or_canonical, resolve_risk_free_rate,
        rounded_price, sortino_ratio, year_end,
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
    fn earnings_date_is_shown_in_eastern_time() {
        // May is daylight saving time: EDT is UTC-4.
        let edt = chrono::Utc.with_ymd_and_hms(2026, 5, 20, 20, 0, 0).unwrap();
        assert_eq!(format_eastern(&edt), "2026-05-20 16:00 ET");

        // February is standard time: EST is UTC-5.
        let est = chrono::Utc.with_ymd_and_hms(2026, 2, 25, 21, 0, 0).unwrap();
        assert_eq!(format_eastern(&est), "2026-02-25 16:00 ET");

        // Converting can change the calendar date.
        let late = chrono::Utc.with_ymd_and_hms(2026, 1, 15, 3, 30, 0).unwrap();
        assert_eq!(format_eastern(&late), "2026-01-14 22:30 ET");

        // DST transition day (2026-03-08 at 2:00 AM local): just before and after.
        let before = chrono::Utc.with_ymd_and_hms(2026, 3, 8, 6, 59, 0).unwrap();
        assert_eq!(format_eastern(&before), "2026-03-08 01:59 ET");
        let after = chrono::Utc.with_ymd_and_hms(2026, 3, 8, 7, 0, 0).unwrap();
        assert_eq!(format_eastern(&after), "2026-03-08 03:00 ET");
    }

    #[test]
    fn sortino_is_na_without_downside() {
        assert_eq!(sortino_ratio(&[], 0.04), None);
        assert_eq!(sortino_ratio(&[0.01, 0.02], 0.0), None);
        assert_eq!(
            format_sortino(&[0.01, 0.02], Some(0.04)),
            "Sortino ratio: n/a (using risk free rate of 4.00%)"
        );
    }

    #[test]
    fn sortino_is_na_without_a_risk_free_rate() {
        assert_eq!(sortino_ratio(&[0.01, -0.02], f64::NAN), None);
        assert_eq!(
            format_sortino(&[0.01, -0.02], None),
            "Risk-free rate unavailable\nSortino ratio: n/a"
        );
    }

    #[test]
    fn sortino_is_some_when_a_day_is_down() {
        let sortino = sortino_ratio(&[0.02, -0.01], 0.0).expect("down day");
        assert!(sortino.is_finite());
        assert!(sortino != 0.0);
    }

    #[test]
    fn risk_free_override_replaces_the_fetched_rate() {
        assert_eq!(resolve_risk_free_rate(Some(0.04), Some(0.01)), Some(0.04));
        assert_eq!(resolve_risk_free_rate(Some(0.04), None), Some(0.04));
        assert_eq!(resolve_risk_free_rate(Some(0.0), Some(0.05)), Some(0.0));
        assert_eq!(resolve_risk_free_rate(None, Some(0.0361)), Some(0.0361));
        assert_eq!(resolve_risk_free_rate(None, None), None);
        assert_eq!(resolve_risk_free_rate(Some(f64::NAN), Some(0.02)), None);
        assert_eq!(resolve_risk_free_rate(None, Some(f64::NAN)), None);
    }

    #[test]
    fn calc_returns_does_not_divide_by_a_zero_close() {
        let quotes = vec![
            candle("9", "11", "10"),
            candle("0", "1", "0"),
            candle("4", "6", "5"),
        ];

        let returns = calc_returns(&quotes);

        assert_eq!(returns.len(), 2);
        assert!((returns[0] - (-1.0)).abs() < 1e-12);
        assert!(returns[1].is_nan());
        assert!(!returns[1].is_infinite());
    }

    #[test]
    fn non_finite_return_prints_as_a_blank_cell() {
        assert_eq!(format_return_cell(f64::NAN), "");
        assert_eq!(format_return_cell(f64::INFINITY), "");
        assert_eq!(format_return_cell(0.2), " 20.00");
        assert_eq!(format_return_cell(-0.25), "-25.00");
    }

    #[test]
    fn money_formatting_falls_back_when_localization_fails() {
        let fcf = Money::new(Decimal::from(96_676_000_000u64), usd_currency()).unwrap();
        let canonical = fcf.to_string();
        assert_ne!(canonical, "$96,676,000,000.00");
        assert_eq!(
            localized_or_canonical(Err(paft_money::MoneyError::InvalidAmountFormat), || {
                canonical.clone()
            }),
            canonical
        );
        assert_eq!(format_money(&fcf), "$96,676,000,000.00");
    }
}
