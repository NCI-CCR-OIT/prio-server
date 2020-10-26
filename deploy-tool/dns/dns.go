package dns

import (
	"fmt"
	"strings"

	"github.com/abetterinternet/prio-server/deploy-tool/config"
	"github.com/abetterinternet/prio-server/deploy-tool/dns/gcloud"
	"github.com/caddyserver/certmagic"
	"github.com/libdns/cloudflare"
	"github.com/libdns/route53"
)

// GetACMEDNSProvider configures an ACMEDNSProvider value to be used in cert generation
func GetACMEDNSProvider(deployConfig config.DeployConfig) (certmagic.ACMEDNSProvider, error) {
	//nolint:gocritic
	switch strings.ToLower(deployConfig.DNS.Provider) {
	case "cloudflare":
		if deployConfig.DNS.CloudflareConfig == nil {
			return nil, fmt.Errorf("cloudflare configuration of the configuration file was nil")
		}
		provider := &cloudflare.Provider{
			APIToken: deployConfig.DNS.CloudflareConfig.APIKey,
		}

		return provider, nil

	case "gcp":
		if deployConfig.DNS.GCPConfig == nil {
			return nil, fmt.Errorf("gcp configuration of the configuration file was nil")
		}
		provider, err := gcloud.NewGoogleDNSProvider(deployConfig.DNS.GCPConfig.Project, deployConfig.DNS.GCPConfig.ZoneMapping)

		if err != nil {
			return nil, err
		}

		return provider, nil

	case "route53":
		if deployConfig.DNS.Route53Config == nil {
			return nil, fmt.Errorf("Route53 configuration of the configuration file was nil")
		}
		provider := &route53.Provider{
			MaxRetries: deployConfig.DNS.Route53Config.MaxRetries,
		}

		return provider, nil
	}

	return nil, fmt.Errorf("no valid provider selected")
}
