import { tool } from '@opencode-ai/plugin'
import { createHash } from 'node:crypto'
import { mkdir, rm, writeFile, access } from 'node:fs/promises'
import { constants } from 'node:fs'
import { join, resolve } from 'node:path'

type CommandResult = {
    code: number
    stdout: string
    stderr: string
}

function runCommand(
    command: string,
    args: string[],
    cwd: string,
    signal: AbortSignal,
): Promise<CommandResult> {
    const subprocess = Bun.spawn({
        cmd: [command, ...args],
        cwd,
        stdin: 'ignore',
        stdout: 'pipe',
        stderr: 'pipe',
        signal,
    })

    return Promise.all([
        new Response(subprocess.stdout).text(),
        new Response(subprocess.stderr).text(),
        subprocess.exited,
    ]).then(([stdout, stderr, code]) => ({
        code,
        stdout,
        stderr,
    }))
}

async function ensureExists(path: string): Promise<void> {
    await access(path, constants.F_OK)
}

function deterministicPrivateKey(seed: string): string {
    return createHash('sha256').update(seed).digest('hex')
}

function delay(ms: number, signal: AbortSignal): Promise<void> {
    return new Promise((resolvePromise, reject) => {
        const timeout = setTimeout(() => {
            signal.removeEventListener('abort', onAbort)
            resolvePromise()
        }, ms)

        const onAbort = () => {
            clearTimeout(timeout)
            reject(new Error('Execution aborted'))
        }

        if (signal.aborted) {
            onAbort()
            return
        }

        signal.addEventListener('abort', onAbort, { once: true })
    })
}

export const itself = tool({
    description: "Interop grandine with itself",
    args: {
        count: tool.schema.number().int().min(1).max(16).describe("Number of grandine instances"),
        durationSeconds: tool.schema.number().int().min(5).max(600).default(60)
            .describe("How long to run instances before stopping")
    },
    async execute(args, context) {
        const quickstartDir = resolve(context.worktree, '..', 'lean-quickstart')
        const generateGenesisScript = join(quickstartDir, 'generate-genesis.sh')

        await ensureExists(quickstartDir)
        await ensureExists(generateGenesisScript)

        const runId = `${Date.now()}-${context.messageID}`
        const networkRoot = join(quickstartDir, '.opencode-itself', runId)
        const configDir = join(networkRoot, 'genesis')
        const dataDir = join(networkRoot, 'data')

        const containerNames: string[] = []
        const instanceNames = Array.from({ length: args.count }, (_, i) => `grandine_${i}`)

        context.metadata({ title: `Starting ${args.count} grandine instance(s)` })

        try {
            context.metadata({ title: 'Building local grandine image (make docker-local)' })
            const buildResult = await runCommand('make', ['docker-local'], context.worktree, context.abort)
            if (buildResult.code !== 0) {
                throw new Error(`local image build failed:\n${buildResult.stdout}\n${buildResult.stderr}`)
            }

            await mkdir(configDir, { recursive: true })
            await mkdir(dataDir, { recursive: true })

            const validatorsYaml = instanceNames
                .map((name, index) => {
                    const privkey = deterministicPrivateKey(`${context.sessionID}:${runId}:${name}:${index}`)
                    const quicPort = 10000 + index
                    const metricsPort = 11000 + index
                    const isAggregator = index === 0 ? 'true' : 'false'
                    return [
                        `  - name: "${name}"`,
                        `    privkey: "${privkey}"`,
                        '    enrFields:',
                        '      ip: "127.0.0.1"',
                        `      quic: ${quicPort}`,
                        `    metricsPort: ${metricsPort}`,
                        `    isAggregator: ${isAggregator}`,
                        '    count: 1',
                    ].join('\n')
                })
                .join('\n')

            const validatorConfig = [
                'shuffle: roundrobin',
                'deployment_mode: local',
                'config:',
                '  activeEpoch: 18',
                '  keyType: "hash-sig"',
                'validators:',
                validatorsYaml,
                '',
            ].join('\n')

            await writeFile(join(configDir, 'validator-config.yaml'), validatorConfig)

            for (let index = 0; index < instanceNames.length; index += 1) {
                const name = instanceNames[index]
                const key = deterministicPrivateKey(`${context.sessionID}:${runId}:${name}:${index}`)
                await writeFile(join(configDir, `${name}.key`), `${key}\n`)
                await mkdir(join(dataDir, name), { recursive: true })
            }

            const genesisResult = await runCommand(
                './generate-genesis.sh',
                [configDir, '--mode', 'local'],
                quickstartDir,
                context.abort,
            )

            if (genesisResult.code !== 0) {
                throw new Error(`genesis generation failed:\n${genesisResult.stdout}\n${genesisResult.stderr}`)
            }

            for (let index = 0; index < instanceNames.length; index += 1) {
                const name = instanceNames[index]
                const quicPort = 10000 + index
                const metricsPort = 11000 + index

                const dockerArgs = [
                    'run',
                    '-d',
                    '--rm',
                    '--name',
                    name,
                    '--network',
                    'host',
                    '-v',
                    `${configDir}:/config`,
                    '-v',
                    `${join(dataDir, name)}:/data`,
                    'sifrai/lean:devnet-3',
                    '--genesis',
                    '/config/config.yaml',
                    '--validator-registry-path',
                    '/config/validators.yaml',
                    '--bootnodes',
                    '/config/nodes.yaml',
                    '--node-id',
                    name,
                    '--node-key',
                    `/config/${name}.key`,
                    '--port',
                    `${quicPort}`,
                    '--address',
                    '0.0.0.0',
                    '--metrics',
                    '--http-address',
                    '0.0.0.0',
                    '--http-port',
                    `${metricsPort}`,
                    '--hash-sig-key-dir',
                    '/config/hash-sig-keys',
                ]

                const startResult = await runCommand('docker', dockerArgs, quickstartDir, context.abort)
                if (startResult.code !== 0) {
                    throw new Error(`failed to start ${name}:\n${startResult.stdout}\n${startResult.stderr}`)
                }
                containerNames.push(name)
            }

            await delay(args.durationSeconds * 1000, context.abort)

            const logs: string[] = []

            for (const name of containerNames) {
                const logResult = await runCommand('docker', ['logs', name], quickstartDir, context.abort)
                const logOutput = [logResult.stdout, logResult.stderr].filter(Boolean).join('\n').trim()
                logs.push(`===== ${name} =====\n${logOutput.length > 0 ? logOutput : '<no logs>'}`)
            }

            return logs.join('\n\n')
        } finally {
            for (const name of containerNames) {
                try {
                    await runCommand('docker', ['rm', '-f', name], quickstartDir, new AbortController().signal)
                } catch {
                    // Best-effort cleanup.
                }
            }
            await rm(networkRoot, { recursive: true, force: true })
        }
    }
});
