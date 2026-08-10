# cellar

[![Read in English](https://img.shields.io/badge/Read%20in-English-0b7285?style=for-the-badge)](README.md)

> This document is also available in [English](README.md).

Um arquivo pesquisável do código-fonte que a Meta entrega aos clientes. Baixe uma
versão do WhatsApp Web, compare com outra, leia qualquer módulo e descubra o que
depende do quê.

O WhatsApp Web é entregue como dezenas de milhares de arquivos JavaScript
minificados. Cada arquivo empacota várias definições `__d("NomeDoModulo", [deps],
factory, id)` em uma única linha. O `cellar` baixa uma versão do cliente, analisa
com a AST do [oxc](https://oxc.rs) e grava um arquivo por módulo, nomeado conforme o
módulo. O resultado é um diretório em que `grep -r`, seu editor e seu agente de IA
funcionam diretamente.

## Recursos

- Baixe e gerencie bundles de qualquer versão anterior do cliente.
- Compare duas versões usando um filtro nomeado, em JSON, NDJSON, Markdown ou texto.
- Busque módulos por nome, pelo código interno ou pelo que exportam.
- Veja o código de qualquer módulo, com dependências, dependentes e exports.
- Monte grafos de dependências e dependentes, em JSON, DOT ou Mermaid.
- Disponibilize tudo isso para um agente de IA via MCP.
- Suporta `whatsapp`, `messenger`, `facebook` e `instagram`.

## Requisitos

- Uma toolchain estável recente do Rust (1.95 ou superior).
- Cerca de 1,3 GB de disco por versão indexada.
- O [`just`](https://github.com/casey/just) é opcional, para os atalhos.

## Instalação

```bash
git clone https://github.com/polymorfa/cellar
cd cellar
cargo install --path crates/cellar
```

## Uso

### Gerenciando bundles

Indexe a versão atual do WhatsApp Web. Isso baixa algumas centenas de megabytes e
leva alguns minutos.

```bash
cellar bundle add --rev latest
cellar bundle add --rev 1030882912
```

Os bundles ficam em `~/.cellar` por padrão. Use `CELLAR_HOME` para mudar isso.

```bash
cellar bundle list
cellar bundle info latest
cellar bundle rm whatsapp-1030882912 --yes
```

Você também pode indexar um arquivo que já tenha, seja um `.zip` ou um diretório com
os chunks já extraídos.

```bash
cellar bundle import --rev 1030882912 --from ./whatsapp-1030882912
```

O comando `cellar bundle info` mostra o diagnóstico da extração. Confira antes de
confiar em um resultado estranho. Um `chunkParseFailures` diferente de zero significa
que faltam módulos no índice, e não que faltam no WhatsApp.

### Buscando módulos

```bash
cellar module search --name '^WAWeb' --source 'addonType' -C 2
cellar grep 'disappearing_mode' --filter protocol
cellar module show WAWebSendMsgStanza
cellar module show WAWebSendMsgStanza --path-only
```

Sempre que possível, restrinja com `--name`. O padrão de nome é aplicado primeiro ao
índice em memória, então só os arquivos que sobram são abertos.

### Comparando versões

```bash
cellar diff --no-hunks
cellar diff whatsapp-1030882912 whatsapp-1044822804 --format json
```

Sem argumentos, o `diff` compara as duas versões mais recentes já armazenadas. Comece
com `--no-hunks` para ter uma visão geral e depois repita para os módulos que valem a
leitura.

Mudanças em que todas as linhas alteradas são saída do transpilador são contadas como
`noiseOnly` no resumo e ficam fora da lista. Use `--include-noise` para vê-las.

### Grafos de dependências

```bash
cellar graph WAWebSendMsgStanza --direction dependents --depth 2
cellar graph --match '^WAWebNewsletter' --direction deps --format mermaid
```

O código minificado referencia dependências por posição (`d[3]`), nunca por nome. Os
arrays de dependências são o único registro de quem usa o quê, e o `cellar` os
inverte no momento da indexação. O grep não responde a essa pergunta.

## Filtros

Um bundle tem cerca de 170.000 módulos. A superfície de protocolo é aproximadamente
11% disso. O resto são componentes React, analytics e código dos outros produtos da
Meta que dividem o mesmo bundle. Sem um filtro, comparar duas versões gera dezenas de
milhares de entradas.

| Filtro | O que mantém | Serve para |
| --- | --- | --- |
| `default` | Superfície de protocolo (~11%) | Comparar releases |
| `all` | Tudo | Verificar se o filtro escondeu sua resposta |
| `protocol` | Só módulos de protocolo confirmados | Resultados curtos e diretos |
| `schemas` | `.pb`, `.proto` e `.graphql` | Novos campos protobuf e novas queries |
| `wam` | Eventos e enums de analytics | Sinais iniciais de recursos não lançados |

O filtro `default` exclui de forma definitiva os módulos `.pb` e `.graphql`, porque
são artefatos gerados que um diff de texto reporta mal. Use `--filter schemas` quando
estiver procurando um campo protobuf novo.

Inspecione um filtro antes de confiar nele e faça um fork se precisar ajustar.

```bash
cellar filter list
cellar filter test default --bundle latest
cellar filter fork default meu
cellar filter set meu --from ./meu.json
```

A precedência é `hardExclude`, depois `include`, depois `exclude` e por fim
`defaultVerdict`. Com `excludeDependentsOfExcluded`, um módulo que depende
transitivamente de um módulo excluído também é excluído. Isso é calculado como ponto
fixo sobre o grafo, então o resultado não depende da ordem em que os módulos foram
visitados.

## Integração com agentes

O comando `cellar mcp` expõe as mesmas operações via MCP, como dezesseis ferramentas.

```bash
just install-claude    # Claude Code
just install-codex     # Codex
just install-agents    # ambos
```

Cada atalho instala o binário, registra o servidor MCP e instala uma skill que ensina
o agente a saber quando usar cada comando. Ferramentas de leitura são marcadas como
tal, `bundle_remove` exige confirmação explícita e `bundle_add` é a única marcada como
acesso à rede.

Todo resultado traz um caminho absoluto, então o agente pode começar com uma consulta
ao `cellar` e continuar com a própria leitura de arquivos e o próprio grep.

## Como funciona

A Meta serve um zip com os chunks de qualquer versão anterior em
`https://www.facebook.com/btarchive/<versao>/<plataforma>`. Não é preciso login. As
URLs do bundle ao vivo deixam de funcionar assim que um novo rollout acontece, então
esse endpoint é o que torna possível comparar releases. O `bundle add` é a única
operação que acessa a rede.

O endpoint exige uma requisição de navegador coerente. A borda da Meta compara o
`User-Agent` com o resto da requisição, e uma requisição que se diz Chrome sem os
cabeçalhos `Sec-Fetch-*` recebe um 400. Veja `NAVIGATION_HEADERS` em `cellar-fetch`
para os status medidos.

Algumas decisões de projeto:

- **AST, não regex.** Os limites dos módulos vêm do parser. Contar chaves atravessando
  strings, templates e literais de regex trunca módulos em silêncio, e um módulo
  truncado aparece como um falso diff na versão seguinte.
- **Saída determinística.** Toda lista é ordenada e todo mapa é um `BTreeMap`, então
  reindexar um bundle produz um JSON idêntico byte a byte. Os chunks são processados
  em paralelo e a atribuição de nomes de arquivo roda em série sobre nomes ordenados.
- **Diagnóstico em vez de silêncio.** Tudo que é visto mas não resolvido é contado no
  `manifest.json`. Resultados truncados dizem isso, e um diff pulado diz o porquê.
- **O código é reimpresso.** Um módulo em uma linha de 200 KB inutiliza grep e diffs
  por linha, então os módulos são impressos a partir da AST. A identidade em bytes é
  guardada à parte como `rawSha256`, então a detecção de mudanças continua exata.
- **As variantes são preservadas.** Cerca de um quarto dos módulos vem com mais de uma
  definição distinta. Todas são gravadas, e a detecção de mudanças considera o
  conjunto inteiro.

## Estrutura do projeto

| Crate | Papel |
| --- | --- |
| `cellar-core` | Layout de armazenamento, modelo do índice, filtros, diff e grafos. Sem I/O além do sistema de arquivos. |
| `cellar-index` | Análise do bundle com oxc até virar um índice de módulos. |
| `cellar-fetch` | Descoberta de versão, download e extração. A única crate que acessa a rede. |
| `cellar` | CLI e servidor MCP, ambos sobre uma única camada de operações. |

## Desenvolvimento

```bash
just ci        # fmt-check, clippy, testes, verificação da skill
just test
just clippy
```

Nenhum teste pode acessar a rede. O CI garante isso.

## Créditos

- O `wa-diff-analyzer` do [ProtoCocktail](https://github.com/purpshell), cujo conjunto
  de filtros de módulo é portado aqui como o filtro `default`.
- O [whatspec](https://github.com/oxidezap/whatspec), de João Lucas, que extrai uma IR
  de protocolo tipada dos mesmos bundles e influenciou o design daqui.
- O [meta-code-verify](https://github.com/facebookincubator/meta-code-verify), a
  extensão de integridade de código da própria Meta, que documenta o endpoint
  `btarchive`.

## Suporte

Se você quer suporte de nível empresarial do Rajeh, é possível agendar uma videochamada.
Reserve um horário de 1 hora entrando em contato com ele no Discord ou fazendo o
pré-agendamento [aqui](https://purpshell.dev/book). Quanto antes você reservar, melhor,
porque os horários costumam encher rápido.

Se você representa uma empresa, incentivamos que contribua de volta com os custos de
desenvolvimento do projeto. Você pode fazer isso agendando reuniões ou patrocinando
abaixo. Todo apoio é bem-vindo, de empresas de qualquer tamanho.

## Patrocínio

Se você quiser apoiar financeiramente este projeto, pode fazer isso
[aqui](https://purpshell.dev/sponsor).

## Aviso legal

> [!CAUTION]
> Este projeto não é afiliado, associado, autorizado, endossado ou de qualquer forma
> oficialmente ligado ao WhatsApp ou a qualquer uma de suas subsidiárias ou afiliadas.
> O site oficial do WhatsApp é whatsapp.com. "WhatsApp", bem como nomes, marcas,
> emblemas e imagens relacionados, são marcas registradas de seus respectivos donos.
>
> O `cellar` lê bundles do cliente servidos publicamente para pesquisa de
> interoperabilidade. Os mantenedores não apoiam o uso deste projeto em práticas que
> violem os Termos de Serviço do WhatsApp, e apelam à responsabilidade pessoal de seus
> usuários para usá-lo de forma justa.

## Licença

Copyright (c) 2026 Rajeh Taher

Licenciado sob a Licença MIT. Veja o [LICENSE](LICENSE) para o texto completo.
