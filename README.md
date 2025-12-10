# LeanEtheriumReengeneer
Block Chain technology university course

Initial 2

tests run by executing in lean_client folder following command:  cargo test --workspace



## Testing against other implementations

1. Clone the quickstart repository as a sibling folder:
   ```bash
   cd ..  # Go to parent directory (same level as this repo)
   git clone https://github.com/blockblaz/lean-quickstart
   cd lean-quickstart
   ```
2. Launch Zeam and Ream nodes:
   ```bash
   NETWORK_DIR=local-devnet ./spin-node.sh --node zeam_0,ream_0 --generateGenesis
   ```
3. Launch the client:
   Once the nodes are running, launch the client using the `.vscode/launch.json` script.
