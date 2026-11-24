# codex_ai

Passerelle Rust locale entre un compte ChatGPT ayant accès à Codex et des clients OpenAI. Elle s’inspire de [`openai_subscribed.rs`](https://github.com/zed-industries/zed/blob/7279c76ef71d45504d7cfea00ee054f852ca0fbf/crates/openai_subscribed/src/openai_subscribed.rs), de [`responses.rs`](https://github.com/zed-industries/zed/blob/7279c76ef71d45504d7cfea00ee054f852ca0fbf/crates/open_ai/src/responses.rs) et de [`completion.rs`](https://github.com/zed-industries/zed/blob/7279c76ef71d45504d7cfea00ee054f852ca0fbf/crates/open_ai/src/completion.rs) de Zed.

Pour installer et gérer le service avec `brew services`, suivez le [guide Homebrew](homebrew/README.md).

## Démarrage (macOS et Linux, Rust 1.89+)

```sh
cargo run --locked -- login
```

Ouvrez l’URL affichée dans un navigateur **sur la même machine**, puis autorisez la connexion. Le callback OAuth utilise le port `1455`, ou `1457` si le premier est occupé. Ensuite :

```sh
export CODEX_API_KEY="$(openssl rand -hex 32)"
cargo run --locked -- serve
```

Configurez votre client OpenAI avec la **base URL `http://127.0.0.1:8787/v1`** et la **clé locale `CODEX_API_KEY`**. Cette clé n’est ni une clé OpenAI ni un jeton OAuth. Pour voir les modèles autorisés sur votre compte (depuis un terminal qui connaît la même clé) :

```sh
curl -fsS http://127.0.0.1:8787/v1/models \
  -H "Authorization: Bearer $CODEX_API_KEY"
```

Pour générer du texte, utilisez un `id` renvoyé par `/v1/models` à la place de `MODELE` :

```sh
curl -fsS http://127.0.0.1:8787/v1/responses \
  -H "Authorization: Bearer $CODEX_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"MODELE","input":"Réponds bonjour."}'
```

## API prise en charge

| Route | Comportement |
| --- | --- |
| `GET /v1/models` | Modèles visibles dans le catalogue du compte Codex. |
| `POST /v1/responses` | Génération JSON ou SSE (`stream: true`). Les items et outils Responses natifs sont transmis à Codex, sous réserve de ce que le backend accepte. |
| `POST /v1/chat/completions` | Texte, appels d’outils de type `function` et usage, en JSON ou SSE. Pour les images et les autres types d’outils, utilisez Responses. |

La passerelle est sans état de conversation : elle envoie `store: false`. Renvoyez l’historique pertinent dans `input` ou `messages` à chaque tour. Pas de `previous_response_id`, de mode `background`, d’embeddings ni de prise en charge complète de l’API OpenAI publique. Les modèles, paramètres et quotas dépendent de votre accès Codex. Le backend ChatGPT utilisé ici n’est pas l’API publique `api.openai.com` et peut changer.

### Tester un outil Chat Completions

```sh
curl -fsS http://127.0.0.1:8787/v1/chat/completions \
  -H "Authorization: Bearer $CODEX_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"MODELE","messages":[{"role":"user","content":"Quelle heure est-il à Paris ?"}],"tools":[{"type":"function","function":{"name":"get_local_time","parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}}],"tool_choice":{"type":"function","function":{"name":"get_local_time"}}}'
```

Remplacez `MODELE` par un `id` de `/v1/models`. Le client reçoit `message.tool_calls`, exécute la fonction et renvoie l’historique avec le message `assistant`, puis un message `tool` dont le `tool_call_id` reprend l’`id` de l’appel. Le connecteur n’exécute pas la fonction. Les appels `function_call` de l’ancienne API Chat et les outils `custom` ne sont pas pris en charge ici.

Pour un client distant qui utilise `stream: true` (comme Cursor Agent), le tunnel doit prendre en charge le SSE. Les Quick Tunnels `trycloudflare.com` ne le prennent pas en charge.

## Mode debug

Démarrez le serveur avec `CODEX_DEBUG=1` pour suivre les requêtes dans le terminal :

```sh
CODEX_DEBUG=1 ./target/debug/codex_ai serve
```

Les événements de diagnostic sont écrits sur `stderr`, avec le même `request` et un `elapsed_ms` relatif au début de chaque requête. Ils indiquent la route, le statut HTTP, le modèle GPT reconnu, le nombre d’outils, l’état du cache OAuth et la fin ou l’erreur du flux Codex. En mode SSE, `http.response` indique l’envoi des en-têtes, pas la fin de la génération : regardez ensuite `codex.stream.end` ou `codex.stream.error`.

Aucune télémétrie n’est envoyée à l’extérieur. Les URL complètes, en-têtes, clés, prompts, réponses et arguments d’outils ne sont pas journalisés par ce mode. Retirez `CODEX_DEBUG=1` et redémarrez pour le désactiver. Avec `brew services`, ajoutez `CODEX_DEBUG=1` au [fichier d’environnement du service](homebrew/README.md), puis relancez le service pour que les logs apparaissent dans son fichier de journal habituel.

## Cache et sécurité

- Les jetons d’accès et de rafraîchissement restent dans un fichier local non chiffré `tokens.json` (permissions `0600`, répertoire privé `0700`). Priorité des chemins : `CODEX_CACHE_DIR` (absolu), `$XDG_CACHE_HOME/codex_ai`, puis `~/.cache/codex_ai`. Ne placez pas ce répertoire dans un dépôt ou un dossier partagé.
- Le serveur réutilise le jeton d’accès, le rafraîchit à l’approche de son expiration ou après un `401` de Codex et remplace le cache atomiquement. Un seul processus peut utiliser ce cache à la fois. Si la session est révoquée, arrêtez le serveur et relancez `login`.
- `CODEX_LISTEN` permet de changer le port ou l’adresse de boucle locale (par exemple `127.0.0.1:8080`). Ne publiez pas ce serveur sur le réseau : toute personne possédant la clé locale peut consommer le quota du compte.
- Pour effacer le cache local, arrêtez le serveur puis lancez `cargo run --locked -- logout`. Cette commande ne révoque pas la session chez OpenAI.

`cargo test --locked` vérifie le cache et les formats HTTP sur un backend simulé, sans compte ChatGPT.
