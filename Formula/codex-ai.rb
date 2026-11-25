class CodexAi < Formula
  desc "Codex AI service"
  homepage "https://github.com/bbrangeo/codex_ai"
  url "https://github.com/bbrangeo/codex_ai/archive/refs/tags/v0.1.2.tar.gz"
  sha256 "45371461ef6bfe5281281ff3514646a49defc71d59184f150a9913cb0c17adc5"
  license "MIT"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
    mv bin/"codex_ai", bin/"codex-ai"

    (bin/"codex-ai-service").write <<~SH
      #!/bin/sh
      set -e
      umask 077

      config_dir="${HOME:?HOME must be set}/.config/codex_ai"
      env_file="$config_dir/env"
      mkdir -p "$config_dir"
      if [ ! -e "$env_file" ]; then
        (set -C; printf '%s\\n' \
          '# Generate a CODEX_API_KEY value with openssl rand -hex 32' \
          '# CODEX_API_KEY=' \
          '# CODEX_LISTEN=127.0.0.1:8787' \
          'CODEX_CHAT_FUNCTION_TOOLS_ONLY=1' > "$env_file")
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
      Run codex-ai-service --help to create ~/.config/codex_ai/env.
      Set CODEX_API_KEY in that file, then run codex-ai-service login
      before starting the service without sudo.
    EOS
  end

  test do
    launcher = bin/"codex-ai-service"
    assert_match "CODEX_API_KEY", shell_output("#{launcher} --help")

    config = testpath/".config/codex_ai/env"
    assert_equal 0600, config.stat.mode & 0777
    assert_match(/^CODEX_CHAT_FUNCTION_TOOLS_ONLY=1$/, config.read)
    File.write(config, <<~SH)
      CODEX_AI_BREW_TEST='loaded with spaces'
      /usr/bin/printenv CODEX_AI_BREW_TEST
      exit
    SH
    assert_equal "loaded with spaces\n", shell_output(launcher.to_s)
  end
end
