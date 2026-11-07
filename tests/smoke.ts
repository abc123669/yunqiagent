import {_electron as electron,expect} from '@playwright/test';
import path from 'node:path';
import fs from 'node:fs/promises';

const root=process.cwd(), work=path.resolve(root,'../../work');
const profiles=(await fs.readdir(work)).filter(p=>p.startsWith('desktop-test-profile-')).sort();
const profile=path.join(work,profiles.at(-1)!);
const app=await electron.launch({args:[root],env:{...process.env,YUNQI_TEST_DATA:profile}});
try {
  const page=await app.firstWindow();
  await expect(page.getByText('Agent 已连接',{exact:true})).toBeVisible({timeout:30000});
  const state=await page.evaluate(()=>window.desktop!.load());
  expect(state.settings.hasPassword).toBe(true);
  expect(state.settings.model).toBe('deepseek-flash');
  console.log('Encrypted credential reload and automatic SSH reconnect: passed');
}finally{await app.close();}

if(process.env.YUNQI_PACKAGED_SMOKE){
  const packaged=await electron.launch({executablePath:path.join(root,'release/win-unpacked/YunqiAgent.exe'),args:[]});
  try {
    expect(await packaged.evaluate(({app})=>app.isPackaged)).toBe(true);
    const page=await packaged.firstWindow();
    await expect(page.getByRole('heading',{name:'想法，从这里生长。'})).toBeVisible();
    await page.getByRole('button',{name:'设置',exact:true}).click();
    await expect(page.getByRole('textbox',{name:'模型',exact:true})).toHaveValue('deepseek-flash');
    console.log('Packaged Windows executable launch, assets and default model: passed');
  }finally{await packaged.close();}
}
