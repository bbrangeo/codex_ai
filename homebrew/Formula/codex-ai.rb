class CodexAi < Formula
  desc "Codex AI service"
  url "file://#{File.expand_path("../codex-ai-0.1.0.tar.gz", __dir__)}"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
    mv bin/"codex_ai", bin/"codex-ai"

    (bin/"codex-ai-service").write <<~SH
      #!/bin/sh
      set -e
      umask 077

      config_dir="${HOME:?HOME must be set}/.config/cosex_ai"
      env_file="$config_dir/env"
      mkdir -p "$config_dir"
      if [ ! -e "$env_file" ]; then
        (set -C; printf '%s\\n' \
          '# CODEX_API_KEY : générez la valeur avec openssl rand -hex 32' \
          '# CODEX_API_KEY=' \
          '# CODEX_LISTEN=127.0.0.1:8787' > "$env_file")
      fi

      set -a
      . "$env_file"
      set +a
      exec "#{opt_bin}/codex-ai" "$@"
    SH
    (bin/"codex-ai-service").chmod 0755
  end

  service do
    run [opt_bin/"codex-ai-service", "serve"]
    log_path var/"log/codex-ai.log"
    error_log_path var/"log/codex-ai.log"
  end

  def caveats
    <<~EOS
      Exécutez codex-ai-service --help pour créer ~/.config/cosex_ai/env,
      renseignez CODEX_API_KEY, puis exécutez codex-ai-service login avant
      de lancer le service sans sudo.
    EOS
  end

  test do
    launcher = bin/"codex-ai-service"
    assert_match "CODEX_API_KEY", shell_output("#{launcher} --help")

    config = testpath/".config/cosex_ai/env"
    assert_equal 0600, config.stat.mode & 0777
    File.write(config, <<~SH)
      CODEX_AI_BREW_TEST='loaded with spaces'
      /usr/bin/printenv CODEX_AI_BREW_TEST
      exit
    SH
    assert_equal "loaded with spaces\n", shell_output(launcher.to_s)
  end
end
