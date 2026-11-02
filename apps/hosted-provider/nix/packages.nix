{
  craneLib,
  individualCrateArgs,
}: {
  hostedProvider = craneLib.buildPackage (
    individualCrateArgs
    // {
      pname = "hosted-provider";
      cargoExtraArgs = "-p hosted-provider";
      meta.mainProgram = "hosted-provider";
    }
  );
}
