# "rest-to-a2a" Policy Definition

This is the policy-definition half of the **REST to A2A Bridge** policy. It declares the GCL schema (`gcl.yaml`) and the Anypoint Exchange asset coordinates (`exchange.json`) that the implementation half (`../rest-to-a2a-flex/`) compiles against.

The policy was created with the Omni Gateway Policy Development Kit (PDK). For the complete PDK documentation, see [PDK Overview](https://docs.mulesoft.com/pdk/latest/policies-pdk-overview).

## Make command reference

This project has a Makefile that includes the goals used during the definition development lifecycle.

*For more information about the Makefile, see [Makefile](https://docs.mulesoft.com/pdk/latest/policies-pdk-create-project#makefile).*

### Build
The `make build` goal compiles the definition of the policy.

### Publish
The `make publish` goal publishes the policy definition asset in Anypoint Exchange as a development (`-DEV`) asset.

### Release
The `make release` goal publishes the policy definition to Anypoint Exchange as a production-ready asset.

### Release Local
The `make release-local` goal publishes the policy definition as a release asset to the local Anypoint Exchange cache. Useful while developing the implementation half.

*For more information about publishing policies, see [Uploading Custom Policies to Exchange](https://docs.mulesoft.com/pdk/latest/policies-pdk-publish-policies).*
