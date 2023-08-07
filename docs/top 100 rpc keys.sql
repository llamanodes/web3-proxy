SELECT SUM(`sum_credits_used`) as sum, rpc_accounting_v2.rpc_key_id, rpc_key.user_id, concat("0x", hex(user.address)) as address, user.email
FROM `rpc_accounting_v2`
JOIN rpc_key ON rpc_accounting_v2.rpc_key_id=rpc_key.id
JOIN user on rpc_key.user_id=user.id
GROUP BY `rpc_key_id`
ORDER BY sum DESC
LIMIT 100