use anyhow::Result;
use chrono::Utc;
use chrono::{DateTime, Datelike, NaiveDate, NaiveTime, Timelike};
use chrono_tz::America::New_York;
use clap::{Parser, ValueEnum};
use num_format::{Locale, ToFormattedString};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use statrs::statistics::Statistics;
use tabled::{builder::Builder, settings::Style};
use textplots::{Chart, Plot, Shape};
use yfinance_rs::fundamentals::CashflowRow;
use yfinance_rs::{Candle, FastInfo, Interval, MarketState, Quote, Range, Ticker, YfClientBuilder};
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

    let (quotes, earnings, quote_info, cash_flow, fetched_risk_free, quote) = tokio::join!(
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
        ticker.quote(),
    );
    // Price history is the body of the report. The other requests are optional
    // and must not discard a history that already succeeded.
    let quotes = quotes?;
    let earnings = earnings.ok();
    let quote_info = quote_info.ok();
    let quote = quote.ok();
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
    print_extended_hours(&extended_hours_section(&ticker, quote.as_ref(), quotes.last()).await);

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionLabel {
    PreMarket,
    AfterHours,
}

impl SessionLabel {
    fn as_str(self) -> &'static str {
        match self {
            SessionLabel::PreMarket => "Pre-market",
            SessionLabel::AfterHours => "After-hours",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ExtendedHours {
    /// Regular session, or the quote did not say which session it is.
    Hidden,
    /// Outside the regular session, but no extended-hours print is available.
    Unavailable,
    Line(String),
}

struct ExtendedBar {
    label: SessionLabel,
    price: Decimal,
    currency: Currency,
    ts: DateTime<Utc>,
}

fn is_regular_session(state: &MarketState) -> bool {
    matches!(state, MarketState::Regular)
}

/// Pre-market is before 9:30 America/New_York. After-hours starts at 16:00.
/// A 1-minute bar timestamped 16:00 is the first post-market minute: Yahoo stamps
/// the start of the interval, and the regular close is the prior minute.
fn extended_session(ts: DateTime<Utc>) -> Option<SessionLabel> {
    let time = ts.with_timezone(&New_York).time();
    let open = NaiveTime::from_hms_opt(9, 30, 0)?;
    let close = NaiveTime::from_hms_opt(16, 0, 0)?;
    if time < open {
        Some(SessionLabel::PreMarket)
    } else if time >= close {
        Some(SessionLabel::AfterHours)
    } else {
        None
    }
}

/// `Pre` and `Post` name the line. Every other non-regular state uses the bar clock.
fn label_extended_bar(state: &MarketState, ts: DateTime<Utc>) -> Option<SessionLabel> {
    let clock = extended_session(ts)?;
    if is_regular_session(state) {
        return None;
    }
    Some(match state {
        MarketState::Pre => SessionLabel::PreMarket,
        MarketState::Post => SessionLabel::AfterHours,
        _ => clock,
    })
}

/// Latest bar outside the regular session. Regular-session bars are ignored.
fn latest_extended_bar(bars: &[Candle], state: &MarketState) -> Option<ExtendedBar> {
    if is_regular_session(state) {
        return None;
    }
    let mut best: Option<&Candle> = None;
    for bar in bars {
        if extended_session(bar.ts).is_none() {
            continue;
        }
        let newer = best.is_none_or(|current| bar.ts >= current.ts);
        if newer {
            best = Some(bar);
        }
    }
    let bar = best?;
    Some(ExtendedBar {
        label: label_extended_bar(state, bar.ts)?,
        price: rounded_price(&bar.ohlc.close, &bar.currency),
        currency: bar.currency.clone(),
        ts: bar.ts,
    })
}

/// Clock time in Eastern, e.g. "7:28 PM ET". The offset follows EST or EDT.
fn format_et_clock(ts: DateTime<Utc>) -> String {
    let local = ts.with_timezone(&New_York);
    let (hour12, meridiem) = match local.hour() {
        0 => (12, "AM"),
        hour @ 1..=11 => (hour, "AM"),
        12 => (12, "PM"),
        hour => (hour - 12, "PM"),
    };
    format!("{hour12}:{:02} {meridiem} ET", local.minute())
}

fn format_extended_price(amount: Decimal, currency: &Currency) -> String {
    match Money::new(amount, currency.clone()) {
        Ok(money) => format_money(&money),
        Err(_) => format!("{amount:.2}"),
    }
}

/// Percent change of `price` against the most recent regular-session close.
fn percent_vs_close(price: Decimal, regular_close: Decimal) -> Option<f64> {
    if regular_close.is_zero() {
        return None;
    }
    let price = to_f64(price);
    let regular_close = to_f64(regular_close);
    if !price.is_finite() || !regular_close.is_finite() || regular_close == 0.0 {
        return None;
    }
    let pct = (price - regular_close) / regular_close * 100.0;
    pct.is_finite().then_some(pct)
}

fn format_extended_hours_line(
    label: SessionLabel,
    price: Decimal,
    currency: &Currency,
    regular_close: Decimal,
    ts: DateTime<Utc>,
) -> Option<String> {
    let pct = percent_vs_close(price, regular_close)?;
    Some(format!(
        "{}: {} ({:+.2}% vs close) as of {}",
        label.as_str(),
        format_extended_price(price, currency),
        pct,
        format_et_clock(ts),
    ))
}

fn extended_hours_line_from_bars(
    bars: &[Candle],
    state: &MarketState,
    regular_close: Decimal,
) -> Option<String> {
    let bar = latest_extended_bar(bars, state)?;
    format_extended_hours_line(bar.label, bar.price, &bar.currency, regular_close, bar.ts)
}

fn bars_on_date(bars: &[Candle], day: NaiveDate) -> bool {
    bars.iter()
        .any(|bar| bar.ts.with_timezone(&New_York).date_naive() == day)
}

/// Use today's 1-minute bars when that session has any, otherwise the wider set.
fn choose_intraday_bars<'a>(
    today_bars: &'a [Candle],
    wider_bars: &'a [Candle],
    today: NaiveDate,
) -> &'a [Candle] {
    if bars_on_date(today_bars, today) {
        today_bars
    } else {
        wider_bars
    }
}

fn extended_hours_output(
    state: Option<&MarketState>,
    bars: &[Candle],
    regular_close: Option<Decimal>,
) -> ExtendedHours {
    let Some(state) = state else {
        return ExtendedHours::Hidden;
    };
    if is_regular_session(state) {
        return ExtendedHours::Hidden;
    }
    let Some(regular_close) = regular_close else {
        return ExtendedHours::Unavailable;
    };
    match extended_hours_line_from_bars(bars, state, regular_close) {
        Some(line) => ExtendedHours::Line(line),
        None => ExtendedHours::Unavailable,
    }
}

fn print_extended_hours(section: &ExtendedHours) {
    match section {
        ExtendedHours::Hidden => {}
        ExtendedHours::Unavailable => println!("Extended hours unavailable"),
        ExtendedHours::Line(line) => println!("{line}"),
    }
}

async fn fetch_extended_bars(ticker: &Ticker) -> Result<Vec<Candle>> {
    let today = Utc::now().with_timezone(&New_York).date_naive();
    // 1-minute bars. `Interval::M1` is one month, not one minute.
    let today_bars = ticker
        .history(Some(Range::D1), Some(Interval::I1m), true)
        .await?;
    if bars_on_date(&today_bars, today) {
        return Ok(today_bars);
    }
    let wider_bars = ticker
        .history(Some(Range::D5), Some(Interval::I1m), true)
        .await?;
    Ok(choose_intraday_bars(&today_bars, &wider_bars, today).to_vec())
}

async fn extended_hours_section(
    ticker: &Ticker,
    quote: Option<&Quote>,
    regular: Option<&Candle>,
) -> ExtendedHours {
    let state = quote.and_then(|q| q.market_state.as_ref());
    let Some(state) = state else {
        return ExtendedHours::Hidden;
    };
    if is_regular_session(state) {
        return ExtendedHours::Hidden;
    }
    let bars = match fetch_extended_bars(ticker).await {
        Ok(bars) => bars,
        Err(_) => return ExtendedHours::Unavailable,
    };
    extended_hours_output(Some(state), &bars, regular.map(close))
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
        bars_on_date, calc_returns, choose_intraday_bars, close, extended_hours_line_from_bars,
        extended_hours_output, extended_session, format_eastern, format_et_clock,
        format_extended_hours_line, format_money, format_return_cell, format_sortino,
        format_volume, get_price_range, localized_or_canonical, percent_vs_close,
        resolve_risk_free_rate, rounded_price, sortino_ratio, year_end, ExtendedHours,
        SessionLabel,
    };
    use chrono::TimeZone;
    use paft_money::{Currency, IsoCurrency, Money, PriceAmount, QuantityAmount};
    use rust_decimal::Decimal;
    use yfinance_rs::{Candle, MarketState, Ohlc};

    fn usd_currency() -> Currency {
        Currency::Iso(IsoCurrency::USD)
    }

    fn price(amount: &str) -> PriceAmount {
        PriceAmount::new(Decimal::from_str_exact(amount).unwrap())
    }

    fn candle(low: &str, high: &str, close: &str) -> Candle {
        let ts = chrono::Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap();
        candle_at(ts, low, high, close)
    }

    fn candle_at(ts: chrono::DateTime<chrono::Utc>, low: &str, high: &str, close: &str) -> Candle {
        let ohlc = Ohlc::new(price(close), price(high), price(low), price(close));
        let mut candle = Candle::new(ts, usd_currency(), ohlc);
        candle.volume = Some(QuantityAmount::from_decimal(Decimal::ONE).unwrap());
        candle
    }

    fn utc(
        year: i32,
        month: u32,
        day: u32,
        hour: u32,
        minute: u32,
    ) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .unwrap()
    }

    fn dec(amount: &str) -> Decimal {
        Decimal::from_str_exact(amount).unwrap()
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
    fn after_hours_line_uses_the_post_state_and_the_latest_extended_bar() {
        // 2026-05-20 23:28 UTC is 7:28 PM EDT. The later regular bar is ignored,
        // and the after-hours bar is not the last element.
        let bars = vec![
            candle_at(utc(2026, 5, 20, 18, 0), "99", "101", "100.10"),
            candle_at(utc(2026, 5, 20, 23, 28), "100", "101", "100.40"),
            candle_at(utc(2026, 5, 20, 19, 59), "99", "101", "100.20"),
        ];

        let line = extended_hours_line_from_bars(&bars, &MarketState::Post, dec("100.00"))
            .expect("after-hours bar");

        assert_eq!(
            line,
            "After-hours: $100.40 (+0.40% vs close) as of 7:28 PM ET"
        );
    }

    #[test]
    fn pre_market_line_uses_the_pre_state() {
        // 2026-05-20 12:15 UTC is 8:15 AM EDT. The later 9:30 bar is the open.
        let bars = vec![
            candle_at(utc(2026, 5, 19, 23, 0), "100", "102", "101.00"),
            candle_at(utc(2026, 5, 20, 12, 15), "99", "100", "99.60"),
            candle_at(utc(2026, 5, 20, 13, 30), "99", "100", "99.70"),
        ];

        let line = extended_hours_line_from_bars(&bars, &MarketState::Pre, dec("100"))
            .expect("pre-market bar");

        assert_eq!(
            line,
            "Pre-market: $99.60 (-0.40% vs close) as of 8:15 AM ET"
        );
    }

    #[test]
    fn closed_and_halted_markets_take_the_label_from_the_bar_clock() {
        let after = [candle_at(utc(2026, 5, 20, 23, 28), "100", "101", "100.40")];
        let pre = [candle_at(utc(2026, 5, 20, 12, 15), "99", "100", "99.60")];
        let both = [
            candle_at(utc(2026, 5, 20, 12, 15), "99", "100", "99.60"),
            candle_at(utc(2026, 5, 20, 23, 28), "100", "101", "100.40"),
        ];

        let after_line =
            extended_hours_line_from_bars(&after, &MarketState::Closed, dec("100")).unwrap();
        let pre_line =
            extended_hours_line_from_bars(&pre, &MarketState::Halted, dec("100")).unwrap();
        let latest =
            extended_hours_line_from_bars(&both, &MarketState::Closed, dec("100")).unwrap();

        assert_eq!(
            after_line,
            "After-hours: $100.40 (+0.40% vs close) as of 7:28 PM ET"
        );
        assert_eq!(
            pre_line,
            "Pre-market: $99.60 (-0.40% vs close) as of 8:15 AM ET"
        );
        assert_eq!(latest, after_line);
    }

    #[test]
    fn extended_hours_line_is_absent_without_an_extended_bar_or_a_close() {
        assert!(extended_hours_line_from_bars(&[], &MarketState::Post, dec("100")).is_none());

        let regular = [candle_at(utc(2026, 5, 20, 18, 0), "99", "101", "100.40")];
        assert!(extended_hours_line_from_bars(&regular, &MarketState::Post, dec("100")).is_none());
        assert!(
            extended_hours_line_from_bars(&regular, &MarketState::Closed, dec("100")).is_none()
        );
        assert!(format_extended_hours_line(
            SessionLabel::AfterHours,
            dec("100.40"),
            &usd_currency(),
            Decimal::ZERO,
            utc(2026, 5, 20, 23, 28),
        )
        .is_none());
        assert_eq!(percent_vs_close(dec("100.40"), Decimal::ZERO), None);
        assert!((percent_vs_close(dec("100.40"), dec("100")).unwrap() - 0.40).abs() < 1e-12);
        assert!((percent_vs_close(dec("99.60"), dec("100")).unwrap() - -0.40).abs() < 1e-12);
    }

    #[test]
    fn regular_session_stays_quiet_and_a_missing_print_is_unavailable() {
        let bars = [candle_at(utc(2026, 5, 20, 23, 28), "100", "101", "100.40")];

        assert_eq!(
            extended_hours_output(None, &bars, Some(dec("100"))),
            ExtendedHours::Hidden
        );
        assert_eq!(
            extended_hours_output(Some(&MarketState::Regular), &bars, Some(dec("100"))),
            ExtendedHours::Hidden
        );
        assert_eq!(
            extended_hours_output(Some(&MarketState::Post), &[], Some(dec("100"))),
            ExtendedHours::Unavailable
        );
        assert_eq!(
            extended_hours_output(Some(&MarketState::Post), &bars, None),
            ExtendedHours::Unavailable
        );
        assert_eq!(
            extended_hours_output(Some(&MarketState::Post), &bars, Some(dec("100"))),
            ExtendedHours::Line(
                "After-hours: $100.40 (+0.40% vs close) as of 7:28 PM ET".to_string()
            )
        );
    }

    #[test]
    fn extended_hours_clock_follows_eastern_time_across_dst() {
        // May is daylight saving time: EDT is UTC-4.
        assert_eq!(format_et_clock(utc(2026, 5, 20, 23, 28)), "7:28 PM ET");
        // January is standard time: EST is UTC-5. 00:28 UTC is 7:28 PM the evening before.
        assert_eq!(format_et_clock(utc(2026, 1, 15, 0, 28)), "7:28 PM ET");
        // Spring forward on 2026-03-08 at 2:00 AM local.
        assert_eq!(format_et_clock(utc(2026, 3, 8, 6, 59)), "1:59 AM ET");
        assert_eq!(format_et_clock(utc(2026, 3, 8, 7, 0)), "3:00 AM ET");
        // Fall back on 2026-11-01: 1:30 AM occurs on both offsets.
        assert_eq!(format_et_clock(utc(2026, 11, 1, 5, 30)), "1:30 AM ET");
        assert_eq!(format_et_clock(utc(2026, 11, 1, 6, 30)), "1:30 AM ET");
        assert_eq!(format_et_clock(utc(2026, 5, 20, 16, 0)), "12:00 PM ET");
        assert_eq!(format_et_clock(utc(2026, 1, 15, 5, 0)), "12:00 AM ET");
    }

    #[test]
    fn session_clock_splits_at_the_open_and_the_close() {
        // EDT boundaries on 2026-05-20.
        assert_eq!(
            extended_session(utc(2026, 5, 20, 13, 29)),
            Some(SessionLabel::PreMarket)
        );
        assert_eq!(extended_session(utc(2026, 5, 20, 13, 30)), None);
        assert_eq!(extended_session(utc(2026, 5, 20, 19, 59)), None);
        assert_eq!(
            extended_session(utc(2026, 5, 20, 20, 0)),
            Some(SessionLabel::AfterHours)
        );
        // The same local times in EST, 2026-01-14.
        assert_eq!(
            extended_session(utc(2026, 1, 14, 14, 29)),
            Some(SessionLabel::PreMarket)
        );
        assert_eq!(extended_session(utc(2026, 1, 14, 14, 30)), None);
        assert_eq!(
            extended_session(utc(2026, 1, 14, 21, 0)),
            Some(SessionLabel::AfterHours)
        );
    }

    #[test]
    fn intraday_range_widens_when_today_has_no_bars() {
        let friday = utc(2026, 5, 15, 20, 0);
        let saturday = chrono::NaiveDate::from_ymd_opt(2026, 5, 16).unwrap();
        let friday_bars = [candle_at(friday, "9", "11", "10")];
        assert!(!bars_on_date(&friday_bars, saturday));
        assert!(bars_on_date(
            &friday_bars,
            chrono::NaiveDate::from_ymd_opt(2026, 5, 15).unwrap()
        ));

        let today = [candle_at(utc(2026, 5, 15, 20, 0), "10", "12", "11")];
        let wider = [candle_at(utc(2026, 5, 14, 20, 0), "8", "10", "9")];
        assert_eq!(
            close(&choose_intraday_bars(&today, &wider, saturday)[0]),
            dec("9")
        );
        assert_eq!(
            close(
                &choose_intraday_bars(
                    &today,
                    &wider,
                    chrono::NaiveDate::from_ymd_opt(2026, 5, 15).unwrap()
                )[0]
            ),
            dec("11")
        );
        assert!(choose_intraday_bars(&[], &[], saturday).is_empty());
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
