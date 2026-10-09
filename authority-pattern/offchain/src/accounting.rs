use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AccountingError {
    #[error("conversion rate must be positive")]
    ZeroRate,
    #[error("cumulative usage decreased")]
    UsageRegression,
    #[error("cumulative usage exceeds credited units")]
    UsageExceedsCredits,
    #[error("coin liability overflow")]
    Overflow,
    #[error("recognized amount does not match prior usage")]
    PriorLiabilityMismatch,
    #[error("refundable amount exceeds original charge")]
    RefundExceedsCharge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettlementDelta {
    pub cumulative_usage: u64,
    pub cumulative_coin_liability: u64,
    pub newly_earned_coin_units: u64,
}

pub fn coin_units_for_usage(credit_units: u64, rate: u64) -> Result<u64, AccountingError> {
    if rate == 0 {
        return Err(AccountingError::ZeroRate);
    }
    if credit_units == 0 {
        return Ok(0);
    }
    (credit_units - 1)
        .checked_div(rate)
        .and_then(|whole| whole.checked_add(1))
        .ok_or(AccountingError::Overflow)
}

pub fn settlement_delta(
    previous_usage: u64,
    previous_liability: u64,
    cumulative_usage: u64,
    credited_units: u64,
    rate: u64,
) -> Result<SettlementDelta, AccountingError> {
    if cumulative_usage < previous_usage {
        return Err(AccountingError::UsageRegression);
    }
    if cumulative_usage > credited_units {
        return Err(AccountingError::UsageExceedsCredits);
    }
    let expected_previous = coin_units_for_usage(previous_usage, rate)?;
    if previous_liability != expected_previous {
        return Err(AccountingError::PriorLiabilityMismatch);
    }
    let cumulative_coin_liability = coin_units_for_usage(cumulative_usage, rate)?;
    let newly_earned_coin_units = cumulative_coin_liability
        .checked_sub(previous_liability)
        .ok_or(AccountingError::UsageRegression)?;
    Ok(SettlementDelta {
        cumulative_usage,
        cumulative_coin_liability,
        newly_earned_coin_units,
    })
}

pub fn refundable_coin_units(
    charged_coin_units: u64,
    cumulative_usage: u64,
    rate: u64,
) -> Result<u64, AccountingError> {
    let liability = coin_units_for_usage(cumulative_usage, rate)?;
    charged_coin_units
        .checked_sub(liability)
        .ok_or(AccountingError::RefundExceedsCharge)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cumulative_rounding_preserves_split_settlement_totals_and_refunds() {
        let rate = 3;
        let mut usage = 0;
        let mut earned = 0;
        for next_usage in [1, 2, 3, 4] {
            let delta = settlement_delta(usage, earned, next_usage, 30, rate).unwrap();
            usage = delta.cumulative_usage;
            earned = delta.cumulative_coin_liability;
        }
        assert_eq!(earned, 2);
        assert_eq!(refundable_coin_units(10, usage, rate).unwrap(), 8);
        assert_eq!(refundable_coin_units(10, 0, rate).unwrap(), 10);
        assert_eq!(coin_units_for_usage(u64::MAX, 1), Ok(u64::MAX));
        assert_eq!(
            settlement_delta(4, 2, 3, 30, rate),
            Err(AccountingError::UsageRegression)
        );
        assert_eq!(
            settlement_delta(0, 0, 31, 30, rate),
            Err(AccountingError::UsageExceedsCredits)
        );
    }

    #[test]
    fn approved_rate_two_oracle_matches_wallet_revenue_and_refund() {
        let delta = settlement_delta(0, 0, 40, 200, 2).unwrap();
        assert_eq!(delta.cumulative_coin_liability, 20);
        assert_eq!(refundable_coin_units(100, 40, 2).unwrap(), 80);
    }
}
