require('dotenv').config();
const hre = require("hardhat");
const {deployCreate2} = require("./utils");

// Constructor args for each executor live in the shared config.
// See config/executor_deployments.json.
const executorDeployments = require("../../config/executor_deployments.json");

// Which protocols to deploy per network. Comment out the protocols you
// don't want to deploy.
const deploy_protocols = {
    "ethereum": [
        "uniswap_v2",
        "ring_swap_v2",
        "pancakeswap_v2",
        "uniswap_v3",
        "uniswap_v4",
        "vm:balancer_v2",
        "ekubo_v2",
        "vm:curve",
        "vm:maverick_v2",
        "vm:balancer_v3",
        "rfq:bebop",
        "rfq:hashflow",
        "fluid_v1",
        "erc4626",
        "rocketpool",
        "ekubo_v3",
        "native_wrapper",
        "rfq:liquorice",
        "vm:fermiswap",
        "vm:liquidityparty",
        "vm:bopamm",
        "rfq:metric",
        "pricelevelstream",
        "rfq:native",
        "sky",
        "lido_v4",
        "etherfi",
        "fallback",
    ],
    "base": [
        "uniswap_v2",
        "uniswap_v3",
        "uniswap_v4",
        "rfq:bebop",
        "aerodrome_slipstreams",
        "aerodrome_v1",
        "native_wrapper",
        "lunarbase",
        "rfq:metric",
        "rfq:native",
        "fallback",
    ],
    "unichain": [
        "uniswap_v2",
        "uniswap_v3",
        "uniswap_v4",
        "vm:curve",
        "velodrome_slipstreams",
        "native_wrapper",
    ],
    "arbitrum": [
        "uniswap_v2",
        "uniswap_v3",
        "uniswap_v4",
        "native_wrapper",
        "rfq:metric",
        "rfq:native"
    ],
    "polygon": [
        "uniswap_v2",
        "uniswap_v3",
        "uniswap_v4",
        "native_wrapper",
        "rfq:metric",
    ],
    "bsc": [
        "uniswap_v2",
        "pancakeswap_v2",
        "uniswap_v3",
        "uniswap_v4",
        "native_wrapper",
        "rfq:metric",
        "rfq:native"
    ],
    "plasma": [
        "uniswap_v3",
        "fluid_v1",
        "vm:curve",
        "native_wrapper",
    ],
    "robinhood": [
        "uniswap_v2",
        "uniswap_v3",
        "uniswap_v4",
        "ekubo_v3",
        "native_wrapper",
        "rfq:metric",
        "rfq:native",
    ],
    "monad": [
        "uniswap_v3",
        "uniswap_v4",
        "vm:balancer_v3",
        "vm:curve",
        "native_wrapper",
        "kuru",
        "vm:hanji",
        "btr_aimm",
    ],
};

async function main() {
    const network = hre.network.name;
    console.log(`Deploying executors to ${network}`);

    const protocols = deploy_protocols[network];
    if (!protocols) {
        throw new Error(`No deploy protocols configured for network: ${network}`);
    }
    const networkDeployments = executorDeployments[network];
    if (!networkDeployments) {
        throw new Error(`No executor deployments configured for network '${network}' in executor_deployments.json`);
    }

    for (const protocol of protocols) {
        const deployment = networkDeployments[protocol];
        if (!deployment) {
            throw new Error(
                `No deployment config for protocol '${protocol}' on network '${network}' in executor_deployments.json`
            );
        }
        const {contract: contractName, args} = deployment;
        // The Blockscout verification path needs the fully qualified name.
        const {sourceName} = await hre.artifacts.readArtifact(contractName);
        const address = await deployCreate2({
            contractName,
            contractFqn: `${sourceName}:${contractName}`,
            args,
            network,
            gasLimit: null,
        });
        console.log(`${protocol}: ${address}`);
    }
}

if (require.main === module) {
    main()
        .then(() => process.exit(0))
        .catch((error) => {
            console.error("Deployment failed:", error);
            process.exit(1);
        });
}

module.exports = {deploy_protocols, executorDeployments};
