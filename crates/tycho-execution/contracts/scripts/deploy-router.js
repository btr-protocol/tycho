require('dotenv').config();
const hre = require("hardhat");
const {deployCreate2, resolveRolesNetwork} = require("./utils");

async function main() {
    const network = hre.network.name;
    // Permit2 is deployed at the same address on all EVM chains
    const permit2 = "0x000000000022D473030F116dDEE9F6B43aC78BA3";
    let feeCalculator = process.env.FEE_CALCULATOR;

    const networkRoles = resolveRolesNetwork(network);
    const unpauser = networkRoles.UNPAUSER_ROLE[0];
    const executorSetter = networkRoles.EXECUTOR_SETTER_ROLE[0];
    const routerFeeSetter = networkRoles.ROUTER_FEE_SETTER[0];

    console.log(`Deploying TychoRouterV3 to ${network} with:`);
    console.log(`- permit2: ${permit2}`);
    console.log(`- feeCalculator: ${feeCalculator}`);
    console.log(`- pauserAdmin: ${unpauser}`);
    console.log(`- unpauserAdmin: ${unpauser}`);
    console.log(`- executorSetterAdmin: ${executorSetter}`);
    console.log(`- routerFeeSetterAdmin: ${routerFeeSetter}`);

    const address = await deployCreate2({
        contractName: "TychoRouterV3",
        contractFqn: "src/TychoRouterV3.sol:TychoRouterV3",
        args: [
            permit2,
            feeCalculator,
            unpauser,
            unpauser,
            executorSetter,
            routerFeeSetter,
        ],
        network,
        gasLimit: null,
    });
    console.log(`TychoRouterV3: ${address}`);
}

main()
    .then(() => process.exit(0))
    .catch((error) => {
        console.error("Deployment failed:", error);
        process.exit(1);
    });