SELECT COUNT(*) FROM rpc_accounting WHERE migrated IS NULL;
UPDATE rpc_accounting SET migrated = NULL;

SELECT SUM(frontend_requests) FROM rpc_accounting;
SELECT SUM(frontend_requests) FROM rpc_accounting_v2;

SELECT SUM(backend_requests) FROM rpc_accounting;
SELECT SUM(backend_requests) FROM rpc_accounting_v2;

SELECT SUM(sum_request_bytes) FROM rpc_accounting;
SELECT SUM(sum_request_bytes) FROM rpc_accounting_v2;

SELECT SUM(sum_response_millis) FROM rpc_accounting;
SELECT SUM(sum_response_millis) FROM rpc_accounting_v2;

SELECT SUM(sum_response_bytes) FROM rpc_accounting;
SELECT SUM(sum_response_bytes) FROM rpc_accounting_v2;
