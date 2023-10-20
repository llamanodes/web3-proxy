wrk.method = "POST"
wrk.body   = "{\"jsonrpc\":\"2.0\",\"method\":\"eth_blockNumber\",\"params\":[],\"id\":420}"
wrk.headers["Content-Type"] = "application/json"

response = function(status, headers, body)
    if status ~= 200 then
      io.write("Status: ".. status .."\n")
      io.write("Body:\n")
      io.write(body .. "\n")
    end
  end
  