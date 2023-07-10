SELECT
    user.id AS user_id,
    COALESCE(SUM(admin_receipt.amount), 0) + COALESCE(SUM(chain_receipt.amount), 0) + COALESCE(SUM(stripe_receipt.amount), 0) + COALESCE(SUM(referee.one_time_bonus_applied_for_referee), 0) + COALESCE(referrer_bonus.total_bonus, 0) AS total_deposits,
    COALESCE(SUM(accounting.sum_credits_used), 0) AS total_spent_including_free_tier,
    COALESCE(SUM(accounting.sum_incl_free_credits_used), 0) AS total_spent_outside_free_tier
FROM
    user
        LEFT JOIN
    admin_increase_balance_receipt AS admin_receipt ON user.id = admin_receipt.deposit_to_user_id
        LEFT JOIN
    increase_on_chain_balance_receipt AS chain_receipt ON user.id = chain_receipt.deposit_to_user_id
        LEFT JOIN
    stripe_increase_balance_receipt AS stripe_receipt ON user.id = stripe_receipt.deposit_to_user_id
        LEFT JOIN
    referee ON user.id = referee.user_id
        LEFT JOIN
    (SELECT referrer.user_id, SUM(referee.credits_applied_for_referrer) AS total_bonus
     FROM referrer
              JOIN referee ON referrer.id = referee.used_referral_code
     GROUP BY referrer.user_id) AS referrer_bonus ON user.id = referrer_bonus.user_id
        LEFT JOIN
    rpc_key ON user.id = rpc_key.user_id
        LEFT JOIN
    rpc_accounting_v2 AS accounting ON rpc_key.id = accounting.rpc_key_id
        LEFT JOIN
    user_tier ON user.user_tier_id = user_tier.id
WHERE
                user.id = 1;