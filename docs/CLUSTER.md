# Cluster (Raft)

Vários hosts rodando o Cardinal podem **compartilhar memória**: o que uma regra grava com `redb_api.set/incr`
e o que o operador define com `heathcliff force` passa a existir em **todos** os nós, com consenso
[Raft](https://raft.github.io/). Se um host cai, outro continua com o mesmo estado — um contador de "3 leituras
quentes seguidas" não zera porque o agente trocou de nó.

Sem `config/raft.json` (nem `CARDINAL_RAFT_PEERS`) nada disso existe: o daemon é um nó único, como antes.

## Configuração: só os IPs

```json
// config/raft.json — em CADA host (a lista pode ser a mesma em todos)
{ "peers": ["10.0.0.11", "10.0.0.12", "10.0.0.13"] }
```

Mais o **segredo do cluster**, igual em todos os nós (`open-cardinal raft keygen` gera um):
`CARDINAL_CLUSTER_SECRET`, `CARDINAL_CLUSTER_SECRET_FILE` ou `config/cluster.secret`. Ele **não é criado
automaticamente**: um nó que inventasse o próprio segredo não falaria com os outros, e uma porta de cluster sem
autenticação deixaria qualquer um na rede reescrever o estado compartilhado.

Só endereços IP são aceitos (`10.0.0.1:50052` ou nomes de host são rejeitados com mensagem clara).
Opcionais: `port` (padrão `50052`, a mesma em todos os hosts), `self_ip`, `cluster_id`, `bind`,
`heartbeat_ms` (200), `election_timeout_ms` (1000), `snapshot_threshold` (10000), `reads`, `tls`.

### O que é descoberto automaticamente

| | Como |
|---|---|
| **Qual IP da lista sou eu** | `self_ip` → um IP da lista que o host possui → *handshake*: disco cada peer e vejo qual responde com o **meu** `instance_id` (funciona atrás de NAT/porta publicada do Docker) → rota de saída |
| **Fundar ou entrar** | Nó em branco sonda os peers: todos em branco (ou já me listam) ⇒ todos fundam com a **mesma** lista; existe um cluster que não me inclui ⇒ peço ao líder para me adicionar |
| **Líder, termo, membros** | `Hello` e o próprio log replicado |
| **Entrada de novos hosts** | Um host novo, configurado com os IPs dos outros, se descobre, entra como *learner* (recebe o log, não vota) e é **promovido automaticamente** quando alcança o líder |
| **Porta** | A mesma em todos (`port`) — por isso basta o IP |

Nós que perdem contato continuam sozinhos até voltarem; ao voltar, alcançam o estado (por log ou por
*snapshot*). Se a lista do `raft.json` divergir do que o cluster já decidiu, **vale o cluster** (o arquivo é só a
semente do primeiro boot).

### Operação

```bash
open-cardinal raft status            # papel, termo, líder, índices, membros
open-cardinal raft members
open-cardinal raft add-peer 10.0.0.14       # (ou simplesmente suba o host novo com os IPs dos outros)
open-cardinal raft remove-peer 10.0.0.12
open-cardinal raft transfer-leader [ip]
open-cardinal raft snapshot          # compacta o log agora
```

Mudanças de membership são **um servidor por vez** (como na tese do Raft) e só são aceitas depois que o novo
líder confirmou uma entrada do próprio termo. Remover o líder: ele continua até a remoção ser confirmada e então se retira; os demais elegem outro.
Ao **parar** um líder (SIGTERM, `open-cardinal stop`) ele antes repassa a liderança a um seguidor atualizado.
Use número **ímpar** de votantes (3 ou 5).

## Modelo de consistência

* **Escritas** (`set`, `incr`, `del`, `heathcliff force`): passam pelo líder e só retornam depois de confirmadas
  por maioria. Um follower encaminha ao líder.
* **Leituras de regras** (`redb_api.get`): **linearizáveis** por padrão (`"reads": "linearizable"`). O nó pergunta
  ao líder o índice de commit atual — o líder o fornece sem rede extra enquanto mantém *lease* com a maioria — e
  espera aplicá-lo localmente. Garante "ler o que acabou de ser escrito em *outro* nó". `"reads": "local"` troca
  isso por velocidade.
* **Overrides do Heathcliff** são lidos localmente a cada pulse: um override confirmado chega aos followers em
  cerca de um RTT; um nó **isolado** do líder usa o último estado conhecido (disponibilidade).
* **Sem quórum**: regras sem estado e overrides já replicados continuam decidindo; regras que precisam
  **escrever** ou **ler de forma consistente** falham *dentro do próprio orçamento de tempo* (sem decisão para
  aquela regra) em vez de travar o pulse. `heathcliff force` retorna erro claro. `/readyz` devolve 503.
* Falha de escrita com resultado **indeterminado** (o líder caiu no meio) é reportada como tal: o cliente não
  é induzido a repetir cegamente uma operação não idempotente.

## Segurança do cluster

* Todo RPC entre nós leva `HMAC(segredo, "ts|ip")` com janela de 60 s (o segredo nunca trafega; relógios devem
  estar sincronizados). Um nó com segredo errado é recusado por todos, com mensagem clara no log.
* `raft.json` → `"tls": {"cert_file", "key_file", "ca_file"}` liga **mTLS** entre os nós (certificados da CA do
  cluster, com o nome DNS `cardinal-raft` — `open-cardinal tls issue` já o inclui).
* Nós só podem pedir para entrar **com o próprio IP autenticado**.
* Divulgue a porta do cluster apenas à rede dos nós (firewall/NetworkPolicy), além da autenticação.

## Implantação

O runtime (daemon, Docker, Swarm, Kubernetes) só muda **padrões** (bind, logs, exigência de autenticação); o
protocolo é o mesmo. Como o nó é identificado pelo **IP** pelo qual os outros o alcançam, o IP precisa ser
*estável*:

| Runtime | Receita | Manifesto |
|---|---|---|
| **Daemon** (hosts) | `raft.json` com os IPs dos hosts; um processo por host | — |
| **Docker** | Rede definida pelo usuário com IPs fixos; `CARDINAL_RAFT_PEERS=172.28.0.11,…` | [`deploy/docker/docker-compose.yml`](../deploy/docker/docker-compose.yml) |
| **Docker Swarm** | `mode: global` em hosts rotulados, **rede `host`** (IP do host é estável; IPs de tasks em overlay não são) | [`deploy/swarm/stack.yml`](../deploy/swarm/stack.yml) |
| **Kubernetes** | `StatefulSet` com `hostNetwork: true`, um pod por nó (IP do nó), PVC, readiness em `/readyz`, PDB `maxUnavailable: 1`, `SIGTERM` → líder repassa a liderança | [`deploy/kubernetes/cardinal.yaml`](../deploy/kubernetes/cardinal.yaml) |

* Em Docker, o `self` é achado pelas interfaces do container; atrás de porta publicada use `self_ip` ou deixe o
  *handshake* resolver.
* Em Kubernetes com rede de pods, use um CNI com IPs fixos (Calico `ipAddrs`, Cilium IPAM): IPs efêmeros não servem
  como identidade Raft.
* Atualizações em sequência: `readinessProbe` em `/readyz` só fica verde quando o nó é membro **e** conhece o
  líder, então um *rolling update* espera o cluster ficar saudável entre um pod e outro.
* Segredos como `Secret`/`docker secret` via `*_FILE`.

> **Validação.** O protocolo, a descoberta e as implantações por **processos** (3 hosts em `127.0.0.1-3` com a
> mesma porta) são cobertos por testes automatizados. Os manifestos de Docker/Swarm/Kubernetes foram escritos
> para esse comportamento e têm a sintaxe validada, mas **não foram executados em um daemon Docker/cluster real**
> neste ambiente — rode-os em homologação antes de produção.

## Como foi verificado

* **Simulação determinística** (`src/raft/sim.rs`): clusters de 3–5 nós sob perda (8 %), duplicação, reordenação,
  crashes, partições, compactação e **mudanças de membership aleatórias**, com as invariantes do Raft checadas a
  cada passo (um líder por termo, *log matching*, *state machine safety*). `RAFT_CHAOS_SEEDS=3000` roda milhares de
  execuções. O soak já encontrou — e corrigimos — problemas reais (snapshot atrasado apagando entradas já
  confirmadas, nó removido nunca sabendo da remoção, deadlock de votos com membership defasada).
* **Processos reais** (`tests/cluster.rs`): eleição, replicação de overrides e de estado de regras, `kill -9` do
  líder (failover ≈ 0,9 s com `election_timeout_ms=600`), reinício em sequência, entrada por IP, *snapshot*,
  segredo/CA errados, sem quórum.
