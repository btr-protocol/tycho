require('dotenv').config();
const hre = require("hardhat");
const {deployCreate2, resolveRolesNetwork} = require("./utils");

async function main() {
    const network = hre.network.name;

    // The routerFeeSetter is the address that will be granted
    // ROUTER_FEE_SETTER_ROLE to manage fee configuration.
    const networkRoles = resolveRolesNetwork(network);
    const routerFeeSetter = networkRoles.ROUTER_FEE_SETTER[0];
    // The routerFeeReceiver owns the vault balance every router fee is credited
    // to. It must be an address that can call withdraw() on the router — the
    // CREATE2 factory below deploys the contract but cannot withdraw, which is
    // why the receiver is a constructor argument rather than the deployer.
    const routerFeeReceiver = networkRoles.ROUTER_FEE_RECEIVER[0];

    console.log(`Deploying FeeCalculator to ${network} with:`);
    console.log(`- routerFeeSetter: ${routerFeeSetter}`);
    console.log(`- routerFeeReceiver: ${routerFeeReceiver}`);

    const address = await deployCreate2({
        contractName: "FeeCalculator",
        contractFqn: "src/FeeCalculator.sol:FeeCalculator",
        args: [routerFeeSetter, routerFeeReceiver],
        network,
    });
    console.log(`FeeCalculator: ${address}`);
}

main()
    .then(() => process.exit(0))
    .catch((error) => {
        console.error("Deployment failed:", error);
        process.exit(1);
    });
