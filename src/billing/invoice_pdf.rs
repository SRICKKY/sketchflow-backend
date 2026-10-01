//! Renders a one-page PDF receipt for a paid invoice.
//!
//! This mirrors the content of the Next.js `buildInvoicePdf` helper
//! (`server/invoicePdf.ts`, built on `pdf-lib`) field-for-field: same page
//! size, same invoice number / billed-to / status / plan / paid-on /
//! coverage / payment-id / amount-paid rows, in the same order. The visual
//! treatment is simplified (no rounded "card" background, no hand-drawn logo
//! mark, label/value columns are left-aligned rather than right-aligned)
//! because the `printpdf` crate used here does not expose the pdf-lib-style
//! rounded-rect/SVG-path helpers or builtin-font text-width metrics that the
//! original design relies on.

use chrono::{DateTime, Utc};
use printpdf::{
    BuiltinFont, Color, Line, LinePoint, Mm, Op, PdfDocument, PdfFontHandle, PdfPage,
    PdfSaveOptions, Point, Pt, Rgb,
};

use super::{BillingInterval, InvoiceRecord};

const PRO_MONTHLY_USD: i64 = 5;
const PRO_YEARLY_USD: i64 = 50;

const PAGE_WIDTH_PT: f32 = 612.0;
const PAGE_HEIGHT_PT: f32 = 792.0;
const LEFT_X: f32 = 56.0;
const VALUE_X: f32 = 340.0;

const INK: Rgb = Rgb {
    r: 17.0 / 255.0,
    g: 24.0 / 255.0,
    b: 28.0 / 255.0,
    icc_profile: None,
};
const MUTED: Rgb = Rgb {
    r: 107.0 / 255.0,
    g: 114.0 / 255.0,
    b: 128.0 / 255.0,
    icc_profile: None,
};
const TEAL: Rgb = Rgb {
    r: 15.0 / 255.0,
    g: 118.0 / 255.0,
    b: 110.0 / 255.0,
    icc_profile: None,
};
const RULE: Rgb = Rgb {
    r: 232.0 / 255.0,
    g: 228.0 / 255.0,
    b: 222.0 / 255.0,
    icc_profile: None,
};

/// Mirrors `pdfAmountLabel` from `server/invoicePdf.ts`.
pub fn pdf_amount_label(amount_minor: i32, currency: &str) -> String {
    let code = currency.trim().to_uppercase();
    let code = if code.is_empty() {
        "INR".to_string()
    } else {
        code
    };
    let amount = amount_minor as f64 / 100.0;
    if code == "INR" {
        format!("Rs. {:.2}", amount)
    } else {
        format!("{} {:.2}", code, amount)
    }
}

/// Mirrors `planLabel` from `lib/billing/invoice.ts`.
fn plan_label(interval: BillingInterval) -> &'static str {
    match interval {
        BillingInterval::Year => "SketchFlow Pro (annual)",
        BillingInterval::Month => "SketchFlow Pro (monthly)",
    }
}

/// Mirrors `planUsdLabel` from `lib/billing/invoice.ts` (`formatUsd` with the
/// default of zero fraction digits, e.g. `$5`).
fn plan_usd_label(interval: BillingInterval) -> String {
    match interval {
        BillingInterval::Year => format!("${} USD / year", PRO_YEARLY_USD),
        BillingInterval::Month => format!("${} USD / month", PRO_MONTHLY_USD),
    }
}

/// Mirrors `formatPlanExpiry` from `lib/billing/subscription.ts`
/// (`Intl.DateTimeFormat("en-US", { dateStyle: "medium", timeZone: "UTC" })`,
/// e.g. `Jan 5, 2026`).
fn format_plan_expiry(date: Option<DateTime<Utc>>) -> String {
    match date {
        Some(date) => date.format("%b %-d, %Y").to_string(),
        None => String::new(),
    }
}

fn text_op(
    x: f32,
    y: f32,
    font: BuiltinFont,
    size: f32,
    color: Rgb,
    text: impl Into<String>,
) -> Vec<Op> {
    vec![
        Op::StartTextSection,
        Op::SetTextCursor {
            pos: Point { x: Pt(x), y: Pt(y) },
        },
        Op::SetFont {
            font: PdfFontHandle::Builtin(font),
            size: Pt(size),
        },
        Op::SetFillColor {
            col: Color::Rgb(color),
        },
        Op::ShowText {
            items: vec![printpdf::TextItem::Text(text.into())],
        },
        Op::EndTextSection,
    ]
}

fn rule_ops(y: f32) -> Vec<Op> {
    vec![
        Op::SetOutlineColor {
            col: Color::Rgb(RULE),
        },
        Op::SetOutlineThickness { pt: Pt(0.8) },
        rule_line(y),
    ]
}

fn rule_line(y: f32) -> Op {
    Op::DrawLine {
        line: Line {
            points: vec![
                LinePoint {
                    p: Point {
                        x: Pt(LEFT_X),
                        y: Pt(y),
                    },
                    bezier: false,
                },
                LinePoint {
                    p: Point {
                        x: Pt(PAGE_WIDTH_PT - LEFT_X),
                        y: Pt(y),
                    },
                    bezier: false,
                },
            ],
            is_closed: false,
        },
    }
}

struct Row {
    label: &'static str,
    value: String,
    sub: Option<String>,
    strong: bool,
}

/// Builds the invoice PDF and returns the raw bytes, ready to be streamed as
/// `application/pdf`.
pub fn build_invoice_pdf(
    invoice: &InvoiceRecord,
    billed_to_name: &str,
    billed_to_email: Option<&str>,
) -> Vec<u8> {
    let interval = BillingInterval::parse(&invoice.interval);
    let mut ops: Vec<Op> = Vec::new();

    ops.extend(text_op(
        LEFT_X,
        740.0,
        BuiltinFont::HelveticaBold,
        18.0,
        INK,
        "SketchFlow",
    ));

    ops.extend(text_op(
        LEFT_X,
        714.0,
        BuiltinFont::HelveticaBold,
        9.0,
        TEAL,
        "INVOICE",
    ));
    ops.extend(text_op(
        VALUE_X,
        714.0,
        BuiltinFont::Helvetica,
        10.0,
        MUTED,
        "Billed to",
    ));

    ops.extend(text_op(
        LEFT_X,
        684.0,
        BuiltinFont::HelveticaBold,
        22.0,
        INK,
        invoice.number.clone(),
    ));
    ops.extend(text_op(
        VALUE_X,
        684.0,
        BuiltinFont::HelveticaBold,
        12.0,
        INK,
        billed_to_name.to_string(),
    ));

    let status_label = if invoice.status == "paid" {
        "Paid".to_string()
    } else {
        invoice.status.clone()
    };
    ops.extend(text_op(
        LEFT_X,
        664.0,
        BuiltinFont::Helvetica,
        10.0,
        MUTED,
        format!("Status: {}", status_label),
    ));
    if let Some(email) = billed_to_email {
        ops.extend(text_op(
            VALUE_X,
            664.0,
            BuiltinFont::Helvetica,
            10.0,
            MUTED,
            email.to_string(),
        ));
    }

    let mut rows = vec![
        Row {
            label: "Plan",
            value: plan_label(interval).to_string(),
            sub: Some(plan_usd_label(interval)),
            strong: false,
        },
        Row {
            label: "Paid on",
            value: format_plan_expiry(invoice.paid_at),
            sub: None,
            strong: false,
        },
        Row {
            label: "Coverage",
            value: format!(
                "{} \u{2013} {}",
                format_plan_expiry(invoice.period_start),
                format_plan_expiry(invoice.period_end)
            ),
            sub: None,
            strong: false,
        },
    ];
    if let Some(payment_id) = &invoice.gateway_payment_id {
        rows.push(Row {
            label: "Payment ID",
            value: payment_id.clone(),
            sub: None,
            strong: false,
        });
    }
    rows.push(Row {
        label: "Amount paid",
        value: pdf_amount_label(invoice.amount_minor, &invoice.currency),
        sub: None,
        strong: true,
    });

    let mut y = 628.0_f32;
    for row in rows {
        ops.extend(rule_ops(y + 22.0));

        let (label_font, label_color) = if row.strong {
            (BuiltinFont::HelveticaBold, INK)
        } else {
            (BuiltinFont::Helvetica, MUTED)
        };
        let value_font = if row.strong {
            BuiltinFont::HelveticaBold
        } else {
            BuiltinFont::Helvetica
        };
        let size = if row.strong { 12.0 } else { 11.0 };
        let text_y = if row.sub.is_some() { y + 6.0 } else { y };

        ops.extend(text_op(
            LEFT_X,
            text_y,
            label_font,
            size,
            label_color,
            row.label,
        ));
        ops.extend(text_op(VALUE_X, text_y, value_font, size, INK, row.value));

        if let Some(sub) = row.sub {
            ops.extend(text_op(
                VALUE_X,
                y - 10.0,
                BuiltinFont::Helvetica,
                9.0,
                MUTED,
                sub,
            ));
            y -= 44.0;
        } else {
            y -= 36.0;
        }
    }

    let page = PdfPage::new(
        Mm::from(Pt(PAGE_WIDTH_PT)),
        Mm::from(Pt(PAGE_HEIGHT_PT)),
        ops,
    );
    let mut doc = PdfDocument::new("SketchFlow Invoice");
    let mut warnings = Vec::new();
    doc.with_pages(vec![page])
        .save(&PdfSaveOptions::default(), &mut warnings)
}
