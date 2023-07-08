-- Notebook to write all sql items (incl. joins)
CREATE VIEW balance_v2 AS
SELECT
    u.id AS user_id,
    COALESCE(SUM(aibr.amount), 0) + COALESCE(SUM(iocbr.amount), 0) + COALESCE(SUM(sibr.amount), 0) + COALESCE(SUM(ref.credits_applied_for_referrer), 0) AS total_deposits,
    COALESCE(SUM(racv2.sum_credits_used), 0) AS total_spent_outside_free_tier
    -- COALESCE(SUM(racv2.sum_credits_used), 0) - COALESCE(SUM(ut.free_tier_credits), 0) AS total_spent_including_free_tier
FROM
    user AS u
        LEFT JOIN
    admin_increase_balance_receipt AS aibr ON u.id = aibr.deposit_to_user_id
        LEFT JOIN
    increase_on_chain_balance_receipt AS iocbr ON u.id = iocbr.deposit_to_user_id
        LEFT JOIN
    stripe_increase_balance_receipt AS sibr ON u.id = sibr.deposit_to_user_id
        LEFT JOIN
    referee AS ref ON u.id = ref.user_id
        LEFT JOIN
    referrer AS rfr ON ref.used_referral_code = rfr.id
        LEFT JOIN
    rpc_key AS rk ON u.id = rk.user_id
        LEFT JOIN
    rpc_accounting_v2 AS racv2 ON rk.id = racv2.rpc_key_id
        LEFT JOIN
    user_tier AS ut ON u.user_tier_id = ut.id;