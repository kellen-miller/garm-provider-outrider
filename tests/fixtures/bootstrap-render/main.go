package main

import (
	"encoding/json"
	"fmt"
	"os"

	"github.com/cloudbase/garm-provider-common/cloudconfig"
	"github.com/cloudbase/garm-provider-common/params"
)

type RenderRequest struct {
	Instance params.BootstrapInstance         `json:"instance"`
	Tools    params.RunnerApplicationDownload `json:"tools"`
}

type RenderResult struct {
	InstallScript string `json:"install_script"`
	CloudConfig   string `json:"cloud_config"`
}

func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func run() error {
	var request RenderRequest
	if err := json.NewDecoder(os.Stdin).Decode(&request); err != nil {
		return fmt.Errorf("decode render request: %w", err)
	}

	script, err := cloudconfig.GetRunnerInstallScript(request.Instance, request.Tools, request.Instance.Name)
	if err != nil {
		return fmt.Errorf("render install hook: %w", err)
	}

	config, err := cloudconfig.GetCloudConfig(request.Instance, request.Tools, request.Instance.Name)
	if err != nil {
		return fmt.Errorf("render provider cloud config: %w", err)
	}

	return json.NewEncoder(os.Stdout).Encode(RenderResult{
		InstallScript: string(script),
		CloudConfig:   config,
	})
}
