# Frequently Asked Questions

## Q) My wallet sits on "sending transaction" a lot longer than on other services.

We send your transactions to multiple private relays to get them mined without exposing them to sandwich attacks.

We have plans to return after the first successful response, but that won't get your transaction confirmed any faster.

Soon, you can opt out of this behavior and we will broadcast your transactions publicly.

## !) How do I sign a login message with cURL?

```
    curl -d '{
    "address": "0x22fbd6248cb2837900c3fe69f725bc02dd3a3b33",
    "msg": "0x73746167696e672e6c6c616d616e6f6465732e636f6d2077616e747320796f7520746f207369676e20696e207769746820796f757220457468657265756d206163636f756e743a0a3078323246624436323438634232383337393030633366653639663732356263303244643341334233330a0af09fa699f09fa699f09fa699f09fa699f09fa6990a0a5552493a2068747470733a2f2f73746167696e672e6c6c616d616e6f6465732e636f6d2f0a56657273696f6e3a20310a436861696e2049443a20310a4e6f6e63653a203031474654345052584342444157355844544643575957354a360a4973737565642041743a20323032322d31302d32305430373a32383a34342e3937323233343730385a0a45787069726174696f6e2054696d653a20323032322d31302d32305430373a34383a34342e3937323233343730385a",
    "sig": "08478ba4646423d67b36b26d60d31b8a54c7b133a5260045b484df687c1fe8f4196dc69792019852c282fb2a1b030be130ef5b78864fff216cdd0c71929351761b",
    "version": "3",
    "signer": "MEW"
    }' -H "Content-Type: application/json" --verbose "http://127.0.0.1:8544/user/login?invite_code=XYZ"
```
