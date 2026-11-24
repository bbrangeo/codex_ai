# Installation avec Homebrew

La formule `codex-ai` compile le projet Rust depuis une archive locale. Depuis la racine du dépôt :

```sh
tap_dir="$(brew --repository local/codex-ai)"
mkdir -p "$tap_dir/Formula"
cp homebrew/Formula/codex-ai.rb "$tap_dir/Formula/"
tar -czf "$tap_dir/codex-ai-0.1.0.tar.gz" Cargo.toml Cargo.lock src
brew trust --formula local/codex-ai/codex-ai
brew install local/codex-ai/codex-ai
```

`brew trust` est nécessaire sur les versions de Homebrew qui exigent d'approuver les formules tierces. Pour reconstruire une version locale modifiée, recopiez la formule, régénérez l'archive, puis lancez `brew fetch --force --build-from-source local/codex-ai/codex-ai` et `brew reinstall local/codex-ai/codex-ai`.

## Configuration et service

1. Lancez `codex-ai-service --help` pour créer `~/.config/cosex_ai/env` s'il n'existe pas (permissions `0600`).
2. Générez une clé locale avec `openssl rand -hex 32`, puis renseignez `CODEX_API_KEY` dans ce fichier. Ce n'est pas une clé OpenAI. `CODEX_LISTEN` est facultatif : par défaut, le serveur écoute sur `127.0.0.1:8787`.
3. Exécutez `codex-ai-service login`, puis ouvrez l'URL affichée pour connecter votre compte ChatGPT. Si vous devez vous reconnecter plus tard, arrêtez d'abord le service : il verrouille le cache de connexion.
4. Démarrez le service avec `brew services start codex-ai`, sans `sudo`.

L'API est accessible par défaut à `http://127.0.0.1:8787/v1`. Utilisez `CODEX_API_KEY` comme clé Bearer dans votre client. Les journaux sont dans `$(brew --prefix)/var/log/codex-ai.log`.

Le lanceur lit le fichier `env` à chaque démarrage sans l'écraser. Écrivez une affectation par ligne (`NOM=valeur` ou `NOM='valeur avec espaces'`). Le fichier est lu comme du shell : n'y copiez que du contenu de confiance et ne le versionnez pas. Après modification, lancez `brew services restart codex-ai`. Pour arrêter le service, utilisez `brew services stop codex-ai` ; pour exécuter le serveur au premier plan, utilisez `codex-ai-service serve`.

Pour publier la formule, remplacez son URL `file://` par l'URL d'une archive de release et ajoutez son `sha256`, puis déposez la formule dans le dossier `Formula/` d'un tap Homebrew publié.
