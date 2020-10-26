# Notes on DNS configuration

## AWS Route53

For AWS Route53 support:

1. The deploy-tool configuration needs to be set up to use Route53 with the proper basic config:
```
[dns]
provider = "route53"

[dns.route53]
"max-retries" = 5
```
2. The [deploy-tool environment needs to be configured](https://docs.aws.amazon.com/sdk-for-go/v1/developer-guide/configuring-sdk.html) with the proper credentials and region information.
3. The [proper IAM policy needs to grant](https://github.com/libdns/route53/blob/master/README.md) to grant permissions to the credentials to manage DNS records.
